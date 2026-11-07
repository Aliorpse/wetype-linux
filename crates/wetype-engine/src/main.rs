//! `wetype-engine.exe` - the process that owns `libwxhld.dll`.
//!
//! Spawned by `wetype-daemon` under Wine. It speaks one protocol over
//! stdin/stdout and nothing else: engine logs go to **stderr**, because stdout
//! is the frame channel.
//!
//! The loop is deliberately **blocking and single-threaded**. The engine's
//! session listener fires synchronously inside `wxime_process_input`, so a
//! request's candidate snapshot is already complete when the call returns;
//! introducing an async runtime here would only move the callback onto a
//! different thread than the caller for no benefit. `tokio` lives in the daemon
//! and the addon instead.

mod api;
mod session;
mod setup;

use std::io::{Read, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use bytes::BytesMut;
use prost::Message;
use wetype_types::codec::{MAX_FRAME_LEN, decode_frame, encode_frame, ipc_codec};
use wetype_types::ipc;

use api::EngineApi;

/// Shut the engine down and let the process end normally.
///
/// A bare `return` from `main` runs `ExitProcess`, which deadlocks inside the
/// loader lock once the engine's worker threads exist - measured repeatedly.
/// `wxime_finalize` is the engine's own teardown: it stops those threads, and
/// after it the process exits cleanly (measured: status 0).
///
/// There is deliberately no hard-kill fallback: an engineered timeout here
/// would only mask a future regression, and if `ExitProcess` ever hangs again
/// the supervisor (`wetype-daemon`, later systemd) is the right place to notice
/// and act - not a thread inside the victim process.
fn shutdown(api: &EngineApi, code: u32) -> ! {
    use std::io::Write;
    std::io::stdout().flush().ok();
    std::io::stderr().flush().ok();

    tracing::debug!("calling wxime_finalize");
    // SAFETY: finalize takes no arguments; nothing below touches the engine.
    unsafe { (api.finalize)() };
    tracing::debug!("wxime_finalize returned");

    std::process::exit(code as i32)
}

fn main() -> Result<()> {
    // Logs to stderr, never stdout - stdout is the protocol channel.
    // `WETYPE_LOG=debug|trace` raises verbosity; the listener event dump
    // (`WETYPE_DEBUG_EVENTS=1`) only shows up at debug.
    let level = match std::env::var("WETYPE_LOG").as_deref() {
        Ok("trace") => tracing::Level::TRACE,
        Ok("debug") => tracing::Level::DEBUG,
        _ => tracing::Level::INFO,
    };
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_max_level(level)
        .init();

    let engine_dir: PathBuf = std::env::var_os("WETYPE_ENGINE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let dll = std::env::var_os("WETYPE_DLL")
        .map(PathBuf::from)
        .unwrap_or_else(|| engine_dir.join("libwxhld.dll"));
    let work_dir = std::env::var_os("WETYPE_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| engine_dir.join("data"));

    tracing::info!(dll = %dll.display(), "loading engine");
    let api = EngineApi::load(&dll).with_context(|| format!("while loading {}", dll.display()))?;
    tracing::info!(version = %api.version(), "engine loaded");

    // Ask the engine to confirm our struct sizes. It throws on mismatch, which
    // is a loud crash - deliberately better than silently writing the wrong
    // layout into engine memory.
    tracing::debug!("wxime_size_check(0xD0, 0x138)");
    // SAFETY: both values are the sizes this build was written against.
    let sized = unsafe { (api.size_check)(0xD0, 0x138) };

    // Everything from here on starts engine threads, so a failure must not fall
    // out of `main`: the Rust runtime would call `ExitProcess` with those
    // threads still alive, which is exactly the loader-lock deadlock described
    // above. Running the bring-up inside a closure routes every error through
    // `shutdown` and therefore through `wxime_finalize`.
    let result = (|| -> Result<()> {
        // A size mismatch means every struct we hand over has the wrong layout.
        // Continuing would write into the wrong offsets with no visible error,
        // so refuse to start at all.
        if sized == 0 {
            bail!("engine rejected InitParam/SessionConfig sizes");
        }
        setup::initialize(&api, &engine_dir, &work_dir).context("engine bring-up failed")?;
        session::install_hooks(&api);
        serve(&api)
    })();
    match &result {
        Ok(()) => tracing::info!("protocol loop finished"),
        Err(e) => tracing::error!(error = %e, "protocol loop failed"),
    }
    // Never return normally: see shutdown.
    shutdown(&api, if result.is_ok() { 0 } else { 1 });
}

/// Read exactly one frame, growing `buf` as needed.
///
/// Returns `Ok(None)` on clean EOF, which is how the daemon says "shut down".
fn read_frame(
    input: &mut impl Read,
    buf: &mut BytesMut,
    codec: &mut tokio_util::codec::LengthDelimitedCodec,
) -> Result<Option<bytes::Bytes>> {
    loop {
        if let Some(frame) = decode_frame(buf, codec).context("framing error")? {
            return Ok(Some(frame));
        }
        let mut tmp = [0u8; 8192];
        let n = input.read(&mut tmp).context("read from stdin")?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

fn write_response(out: &mut impl Write, resp: &ipc::Response) -> Result<()> {
    // The peer rejects frames above MAX_FRAME_LEN and tears the connection
    // down, so never put one on the wire. A pathological engine can hand back
    // huge candidate text; answering with a tiny failure keeps the stream in
    // sync instead of desynchronising both ends.
    if resp.encoded_len() > MAX_FRAME_LEN {
        tracing::error!(
            seq = resp.seq,
            len = resp.encoded_len(),
            "reply exceeds the frame limit"
        );
        return write_response(out, &fail(resp.seq, "reply exceeds the frame limit"));
    }
    let mut buf = BytesMut::new();
    encode_frame(resp, &mut buf).context("encode response")?;
    out.write_all(&buf).context("write to stdout")?;
    out.flush().context("flush stdout")?;
    Ok(())
}

/// Remove the part of a whole-phrase commit that a partial selection already
/// handed to the application. The engine's final event contains the entire
/// phrase (selected prefix + new suffix), while the client has the prefix.
///
/// Returns false when a non-empty prefix did not match. The caller decides
/// whether that is expected: a partial commit is the new segment and normally
/// does not start with the prefix, while a final whole-phrase commit should.
fn subtract_committed_prefix(text: &mut Vec<u8>, prefix: &[u8]) -> bool {
    if prefix.is_empty() {
        return true;
    }
    if !text.starts_with(prefix) {
        return false;
    }
    text.drain(..prefix.len());
    true
}

fn ok(seq: u64, kind: ipc::response::Kind) -> ipc::Response {
    ipc::Response {
        seq,
        ok: true,
        error: String::new(),
        kind: Some(kind),
    }
}

fn fail(seq: u64, msg: impl Into<String>) -> ipc::Response {
    ipc::Response {
        seq,
        ok: false,
        error: msg.into(),
        kind: None,
    }
}

/// Cap per reply. fcitx5 shows a handful per page; the probe's 50-per-event was
/// only for exploration and made every keystroke carry tens of kilobytes.
const MAX_CANDIDATES: usize = 30;
/// Upper bound on concurrent sessions. fcitx5 creates one per input context and
/// keeps it until that context is destroyed, so the ceiling tracks the number
/// of windows that have composed at least once, not the number of keystrokes.
/// 256 leaves room for a very busy desktop without letting a runaway client
/// allocate engine state without bound.
const MAX_SESSIONS: usize = 256;
const MAX_APP_NAME: usize = 256;
/// Upper bound on a single backspace request.
const MAX_BACKSPACES: u32 = 64;

fn serve(api: &EngineApi) -> Result<()> {
    let mut codec = ipc_codec();
    let mut buf = BytesMut::new();
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    let mut sessions = session::Sessions::default();

    loop {
        let Some(frame) = read_frame(&mut stdin, &mut buf, &mut codec)? else {
            tracing::info!("stdin closed, exiting");
            return Ok(());
        };
        let req = match ipc::Request::decode(frame) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "undecodable request");
                continue;
            }
        };
        let seq = req.seq;
        let resp = dispatch(api, &mut sessions, req).unwrap_or_else(|e| fail(seq, e.to_string()));
        write_response(&mut stdout, &resp)?;
    }
}

fn dispatch(
    api: &EngineApi,
    sessions: &mut session::Sessions,
    req: ipc::Request,
) -> Result<ipc::Response> {
    let seq = req.seq;
    let Some(kind) = req.kind else {
        bail!("request without a kind");
    };
    use ipc::request::Kind;

    Ok(match kind {
        Kind::Ping(p) => {
            if p.protocol_version != wetype_types::PROTOCOL_VERSION {
                return Ok(fail(
                    seq,
                    format!(
                        "protocol mismatch: client {} host {}",
                        p.protocol_version,
                        wetype_types::PROTOCOL_VERSION
                    ),
                ));
            }
            ok(
                seq,
                ipc::response::Kind::Pong(ipc::Pong {
                    protocol_version: wetype_types::PROTOCOL_VERSION,
                    engine_version: api.version(),
                }),
            )
        }

        Kind::NewSession(n) => {
            if sessions.len() >= MAX_SESSIONS {
                return Ok(fail(seq, format!("too many sessions (max {MAX_SESSIONS})")));
            }
            if n.app_name.len() > MAX_APP_NAME {
                return Ok(fail(seq, format!("app_name exceeds {MAX_APP_NAME} bytes")));
            }
            let shared = session::Shared::new();
            let app = if n.app_name.is_empty() {
                session::DEFAULT_APP
            } else {
                n.app_name.as_bytes()
            };
            let (id, name) = session::create(api, &shared, app);
            if id <= 0 {
                return Ok(fail(seq, format!("create_session returned {id}")));
            }
            sessions.insert(id, shared, name);
            tracing::info!(session = id, ctx_id = n.ctx_id, "session created");
            ok(
                seq,
                ipc::response::Kind::Created(ipc::SessionCreated { session: id as u64 }),
            )
        }

        Kind::CloseSession(c) => {
            let id = c.session as i64;
            if sessions.get(id).is_none() {
                return Ok(fail(seq, format!("unknown session {id}")));
            }
            // Destroy FIRST: the map entry owns the `Arc<Shared>` whose raw
            // pointer is registered as the listener payload, plus the app-name
            // buffer referenced by SessionConfig. Dropping it before the engine
            // has let go of the session leaves both dangling (the listener can
            // still fire from inside destroy_session). The reference
            // implementation erases only after destroy for the same reason.
            // SAFETY: id came from this process's create_session.
            unsafe { (api.destroy_session)(id) };
            sessions.remove(id);
            tracing::info!(session = id, "session closed");
            ok(seq, ipc::response::Kind::Ack(ipc::Ack {}))
        }

        Kind::Key(k) => {
            let id = k.session as i64;
            if sessions.get(id).is_none() {
                return Ok(fail(seq, format!("unknown session {id}")));
            }
            // Exactly one printable, non-space ASCII byte. Anything else - a
            // whole syllable, a multi-byte codepoint, a stray 0x80 - makes the
            // engine throw a C++ exception that kills this process, and the
            // engine is shared by every session.
            //
            // Space is excluded on purpose: it is the one printable byte the
            // engine cannot handle (measured: 0xe06d7363). No frontend has a
            // reason to send it - Enter/Space are resolved client-side - so the
            // host rejects it rather than letting a new caller crash the shared
            // engine.
            if k.utf8.len() != 1 || !(0x21..=0x7e).contains(&k.utf8[0]) {
                return Ok(fail(
                    seq,
                    "Key.utf8 must be exactly one printable ASCII byte, not space",
                ));
            }
            // SAFETY: the bytes are a valid UTF-8 slice owned by `k` for the
            // duration of the call; the engine does not retain it.
            unsafe {
                (api.process_input)(
                    id as u64,
                    k.utf8.as_ptr() as *const std::ffi::c_char,
                    k.utf8.len() as u32,
                    std::ptr::null(),
                    0,
                );
            }
            let snap = sessions.snapshot(id).unwrap_or_default();
            // Commits are drained, not cloned: each one is delivered once.
            let mut commit = sessions.take_commits(id).concat();
            // If an auto-commit ends a composition that had a partially
            // committed prefix, the engine's text is the whole phrase; only
            // the new tail belongs to the application.
            let prefix = sessions.committed_prefix(id);
            let had_commit = !commit.is_empty();
            let prefix_matched = subtract_committed_prefix(&mut commit, &prefix);
            if snap.selected.is_empty() && snap.raw.is_empty() {
                if had_commit && !prefix.is_empty() && !prefix_matched {
                    // The final text did not start with what was already
                    // committed. The bytes are left untouched, but this is the
                    // case that would duplicate the prefix; keep it text-free.
                    tracing::debug!(
                        session = id,
                        prefix_len = prefix.len(),
                        commit_len = commit.len(),
                        "final commit does not start with the committed prefix"
                    );
                }
                sessions.clear_committed_prefix(id);
            }
            let mut wire = session::to_wire(id, &snap, MAX_CANDIDATES);
            wire.commit = commit;
            ok(seq, ipc::response::Kind::Candidates(wire))
        }

        Kind::Select(s) => {
            let id = s.session as i64;
            let Some(snap) = sessions.snapshot(id) else {
                return Ok(fail(seq, format!("unknown session {id}")));
            };
            if s.generation != snap.generation {
                return Ok(fail(
                    seq,
                    format!(
                        "stale generation: request {} but current {}",
                        s.generation, snap.generation
                    ),
                ));
            }
            let Some(cand) = snap.items.get(s.index as usize).cloned() else {
                return Ok(fail(seq, format!("index {} out of range", s.index)));
            };
            // An empty candidate means the iterator handed us a blank row; the
            // engine throws if that is selected.
            if cand.text.is_empty() {
                return Ok(fail(seq, "candidate has no text"));
            }
            // `cover` counts characters of `raw`, so it tells exactly how much
            // of the pinyin this candidate consumes. Only split when what is
            // left is a key sequence the engine accepts; anything else (0, the
            // 0xFFFFFFFF marker from pseudo candidates, a non-key byte) falls
            // back to whole-word selection.
            let covered = cand.cover as usize;
            let rest_keys = snap.raw.get(covered..).unwrap_or_default();
            let partial = covered > 0
                && covered < snap.raw.len()
                && rest_keys.iter().all(|b| (0x21..=0x7e).contains(b));
            // One export handles both cases. For a partial candidate the
            // engine keeps the selected word as a type-0 pending node and
            // returns candidates for the uncovered suffix; for a whole-word
            // candidate it emits the whole phrase through event type 2. No
            // reset is needed (and resetting is what used to discard the
            // engine's chance to learn the partial word).
            //
            // SAFETY: text and id are owned blobs copied out of the engine's
            // candidate struct; the spare {ptr,u32} pairs stay NULL for a
            // local candidate (cloud payloads would go there).
            unsafe {
                (api.select_candidate)(
                    id,
                    cand.text.as_ptr() as *const std::ffi::c_char,
                    cand.text.len() as u32,
                    cand.id.as_ptr() as *const std::ffi::c_void,
                    cand.id.len() as u32,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                );
            }
            let emitted = sessions.take_commits(id).concat();
            let from_engine = !emitted.is_empty();
            let mut commit = if emitted.is_empty() {
                cand.text.clone()
            } else {
                emitted
            };
            // The engine's final commit is the whole phrase, including text
            // this host already handed to the application during earlier
            // partial selections. Subtract that prefix so the client does not
            // commit it twice.
            //
            // A partial commit is normally just the new segment, so this call
            // does not match and changes nothing; it is unconditional because
            // the engine may also send a whole-phrase commit here. A match on
            // a partial means the candidate repeated the prefix, which would
            // make the subtraction cut real text; log it instead of relying on
            // that invariant silently.
            let prefix_overlap = !snap.committed_prefix.is_empty()
                && commit.starts_with(&snap.committed_prefix);
            let prefix_matched =
                subtract_committed_prefix(&mut commit, &snap.committed_prefix);
            if partial && prefix_overlap {
                tracing::debug!(
                    session = id,
                    prefix_len = snap.committed_prefix.len(),
                    commit_len = commit.len(),
                    "partial commit starts with the committed prefix"
                );
            }
            // Length only: this process's stderr is the journal, and the
            // committed text is whatever the user typed - often a password or
            // a private message. The text stays out of the log at every level.
            if !partial {
                if from_engine
                    && !snap.committed_prefix.is_empty()
                    && !prefix_matched
                {
                    // A whole-phrase commit from the engine should contain the
                    // prefix earlier partial selections already sent; if it
                    // does not, the client may see that prefix twice. Keep the
                    // text out of the log.
                    tracing::debug!(
                        session = id,
                        prefix_len = snap.committed_prefix.len(),
                        commit_len = commit.len(),
                        "whole-phrase commit does not start with the committed prefix"
                    );
                }
                // A normal selection must clear the engine's composition. If
                // it did not (an unknown/pseudo candidate), reset so the next
                // keystroke cannot append to stale pinyin.
                let after = sessions.snapshot(id).unwrap_or_default();
                if !after.selected.is_empty() || !after.raw.is_empty() {
                    // SAFETY: id is a live session.
                    unsafe { (api.reset_session)(id) };
                }
                sessions.clear_committed_prefix(id);
                tracing::debug!(
                    session = id,
                    index = s.index,
                    len = commit.len(),
                    "selected"
                );
                return Ok(ok(
                    seq,
                    ipc::response::Kind::SelectResult(ipc::SelectResult {
                        commit,
                        finished: true,
                        rest: None,
                    }),
                ));
            }
            // The partial word is now part of the application. Remember it so
            // a later final commit can be reduced to its new suffix. The
            // engine keeps the same prefix in its own pending input and will
            // write both it and the completed phrase to the user dictionary.
            sessions.append_committed_prefix(id, &commit);
            let after = sessions.snapshot(id).unwrap_or_default();
            if after.raw != rest_keys {
                tracing::warn!(
                    session = id,
                    raw = %String::from_utf8_lossy(&after.raw),
                    expected = %String::from_utf8_lossy(rest_keys),
                    "partial selection left an unexpected suffix"
                );
            }
            let rest = session::to_wire(id, &after, MAX_CANDIDATES);
            tracing::debug!(
                session = id,
                index = s.index,
                len = commit.len(),
                suffix_len = rest_keys.len(),
                candidates = after.items.len(),
                "partially selected"
            );
            ok(
                seq,
                ipc::response::Kind::SelectResult(ipc::SelectResult {
                    commit,
                    finished: false,
                    rest: Some(rest),
                }),
            )
        }

        Kind::Reset(r) => {
            let id = r.session as i64;
            if sessions.get(id).is_none() {
                return Ok(fail(seq, format!("unknown session {id}")));
            }
            // SAFETY: id is a live session.
            unsafe { (api.reset_session)(id) };
            sessions.clear_committed_prefix(id);
            ok(seq, ipc::response::Kind::Ack(ipc::Ack {}))
        }

        Kind::Backspace(b) => {
            let id = b.session as i64;
            if sessions.get(id).is_none() {
                return Ok(fail(seq, format!("unknown session {id}")));
            }
            let mut deleted = true;
            // Bounded on purpose: the request comes from a client, and each
            // iteration is an engine call on the shared session.
            let count = b.count.clamp(1, MAX_BACKSPACES);
            for _ in 0..count {
                // SAFETY: (session, flag=1, text=NULL, len=0) is the shape the
                // reference implementation uses; it drops one character.
                let r = unsafe { (api.drop_last)(id as u64, 1, std::ptr::null(), 0) };
                deleted &= r != 0;
                if r == 0 {
                    // Nothing left to delete; stop rather than hammering the
                    // engine.
                    break;
                }
            }
            // Backspace changes the composition, so it has to report the new
            // state: replying with a bare Ack would leave the client showing a
            // stale preedit.
            let snap = sessions.snapshot(id).unwrap_or_default();
            let mut commit = sessions.take_commits(id).concat();
            let prefix = sessions.committed_prefix(id);
            let had_commit = !commit.is_empty();
            let prefix_matched = subtract_committed_prefix(&mut commit, &prefix);
            if snap.selected.is_empty() && snap.raw.is_empty() {
                if had_commit && !prefix.is_empty() && !prefix_matched {
                    // The final text did not start with what was already
                    // committed. The bytes are left untouched, but this is the
                    // case that would duplicate the prefix; keep it text-free.
                    tracing::debug!(
                        session = id,
                        prefix_len = prefix.len(),
                        commit_len = commit.len(),
                        "final commit does not start with the committed prefix"
                    );
                }
                sessions.clear_committed_prefix(id);
            }
            let mut wire = session::to_wire(id, &snap, MAX_CANDIDATES);
            wire.commit = commit;
            if !deleted {
                tracing::debug!(session = id, "drop_last had nothing to delete");
            }
            ok(seq, ipc::response::Kind::Candidates(wire))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::subtract_committed_prefix;

    #[test]
    fn final_commit_is_reduced_to_the_new_suffix() {
        let mut whole = "中国人敏".as_bytes().to_vec();
        assert!(subtract_committed_prefix(&mut whole, "中国人".as_bytes()));
        assert_eq!(whole, "敏".as_bytes());
    }

    #[test]
    fn a_partial_segment_does_not_repeat_the_prefix() {
        let mut partial = "世界".as_bytes().to_vec();
        assert!(!subtract_committed_prefix(&mut partial, "你好".as_bytes()));
        assert_eq!(partial, "世界".as_bytes());
    }

    #[test]
    fn only_a_real_prefix_is_removed() {
        let mut whole = "敏".as_bytes().to_vec();
        assert!(!subtract_committed_prefix(&mut whole, "中国人".as_bytes()));
        assert_eq!(whole, "敏".as_bytes());

        let mut no_prefix = "中国人敏".as_bytes().to_vec();
        assert!(subtract_committed_prefix(&mut no_prefix, b""));
        assert_eq!(no_prefix, "中国人敏".as_bytes());
    }
}

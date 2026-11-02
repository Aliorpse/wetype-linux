//! `wetype-daemon` - owns the Wine-hosted engine and serves the fcitx5 addon.
//!
//! ```text
//! fcitx5 addon --unix socket--> wetype-daemon --stdio pipe--> wetype-engine.exe
//! ```
//!
//! The daemon is the only component that knows about Wine. It also owns session
//! bookkeeping: every session belongs to exactly one connection, a request that
//! names someone else's session is refused, and closing a connection closes the
//! sessions it created. Without that, one crashing addon would leak engine
//! sessions forever and a second client could type into the first one's state.

mod engine;

use std::collections::HashMap;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use wetype_types::codec::ipc_codec;
use wetype_types::ipc;

use engine::Engine;

/// Which connection owns a session.
type Owners = Arc<Mutex<HashMap<u64, u64>>>;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();

    // Absolute paths are not a nicety here: the engine host is spawned with
    // `current_dir(engine_dir)`, so a relative "libs/engine/wetype-engine.exe"
    // would be resolved again from inside `libs/engine` and wine would fail to
    // open it. The engine then re-joins its own DLL and data paths, so the
    // whole chain has to be absolute before the child starts.
    let engine_dir = absolute(
        std::env::var_os("WETYPE_ENGINE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("libs/engine")),
    )?;
    let data_dir = absolute(
        std::env::var_os("WETYPE_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| engine_dir.join("data")),
    )?;
    let engine_exe = engine_dir.join("wetype-engine.exe");
    let socket_path = std::env::var_os("WETYPE_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(default_socket);

    tracing::info!(
        engine = %engine_exe.display(),
        socket = %socket_path.display(),
        "starting"
    );

    let engine = Engine::spawn(engine_exe, engine_dir, data_dir)?;
    let engine = Arc::new(Mutex::new(engine));
    let owners: Owners = Arc::new(Mutex::new(HashMap::new()));

    let listener = bind(&socket_path).await?;
    tracing::info!("listening");

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("cannot install SIGTERM handler")?;

    let mut next_conn: u64 = 0;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _addr) = accepted.context("accept failed")?;
                next_conn += 1;
                let conn = next_conn;
                let engine = Arc::clone(&engine);
                let owners = Arc::clone(&owners);
                tokio::spawn(async move {
                    if let Err(e) = serve_client(stream, conn, engine, owners).await {
                        tracing::warn!(conn, error = %e, "connection ended with error");
                    }
                    tracing::info!(conn, "connection closed");
                });
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("SIGINT received, shutting down");
                break;
            }
            _ = sigterm.recv() => {
                // systemd stops units with SIGTERM, not SIGINT.
                tracing::info!("SIGTERM received, shutting down");
                break;
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                // The child is also killed on a failed call. Treat that poisoned
                // engine like any other child exit and let systemd restart us.
                let dead = {
                    let mut guard = engine.lock().await;
                    guard.exited()
                };
                if let Some(status) = dead {
                    // The daemon is the supervisor. A dead engine means every
                    // session is gone and no request can ever succeed again, so
                    // exit non-zero and let systemd restart the unit instead of
                    // serving errors forever from a half-dead process.
                    tracing::error!(%status, "engine host died, exiting for restart");
                    cleanup_all(&engine, &owners).await;
                    let _ = std::fs::remove_file(&socket_path);
                    std::process::exit(1);
                }
                if engine.lock().await.unusable() {
                    tracing::error!("engine host connection became unusable, exiting for restart");
                    cleanup_all(&engine, &owners).await;
                    let _ = std::fs::remove_file(&socket_path);
                    std::process::exit(1);
                }
            }
        }
    }

    cleanup_all(&engine, &owners).await;
    let _ = std::fs::remove_file(&socket_path);
    engine.lock().await.shutdown().await;
    Ok(())
}

/// One connection's view of the world.
struct Conn {
    id: u64,
    stream: Framed<UnixStream, LengthDelimitedCodec>,
    engine: Arc<Mutex<Engine>>,
    owners: Owners,
    /// Sessions created by this connection, so they can be closed on exit.
    owned: Vec<u64>,
}

async fn serve_client(
    stream: UnixStream,
    id: u64,
    engine: Arc<Mutex<Engine>>,
    owners: Owners,
) -> Result<()> {
    let mut conn = Conn {
        id,
        stream: Framed::new(stream, ipc_codec()),
        engine,
        owners,
        owned: Vec::new(),
    };
    tracing::info!(conn = id, "connection accepted");

    let result = conn.serve_requests().await;
    // Clean up sessions on normal EOF and on read/write failures alike.
    conn.close_owned().await;
    result
}

impl Conn {
    async fn serve_requests(&mut self) -> Result<()> {
        while let Some(frame) = self.stream.next().await {
            let frame = frame.context("read from client")?;
            let req = match ipc::Request::decode(frame) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(conn = self.id, error = %e, "undecodable request");
                    continue;
                }
            };
            let seq = req.seq;
            let resp = self.handle(req).await.unwrap_or_else(|e| ipc::Response {
                seq,
                ok: false,
                error: e.to_string(),
                kind: None,
            });
            // Framed already frames; encode only the message body.
            let mut buf = bytes::BytesMut::with_capacity(resp.encoded_len());
            resp.encode(&mut buf)?;
            self.stream
                .send(buf.freeze())
                .await
                .context("write to client")?;
        }
        Ok(())
    }

    async fn handle(&mut self, req: ipc::Request) -> Result<ipc::Response> {
        let seq = req.seq;
        let Some(kind) = req.kind else {
            bail!("request without a kind");
        };
        use ipc::request::Kind;

        // Session-scoped requests are checked here, once, before anything
        // reaches the engine: a connection may only touch its own sessions.
        let session = match &kind {
            Kind::Key(k) => Some(k.session),
            Kind::Select(s) => Some(s.session),
            Kind::Reset(r) => Some(r.session),
            Kind::Backspace(b) => Some(b.session),
            Kind::CloseSession(c) => Some(c.session),
            _ => None,
        };
        if let Some(session) = session {
            let owners = self.owners.lock().await;
            match owners.get(&session) {
                Some(owner) if *owner == self.id => {}
                Some(_) => bail!("session {session} belongs to another connection"),
                None => bail!("unknown session {session}"),
            }
        }

        // Only CloseSession frees ownership. `Reset` also answers with an Ack,
        // and treating that as a close silently dropped the session from
        // bookkeeping while the engine kept it alive.
        let closing = matches!(kind, ipc::request::Kind::CloseSession(_));

        let mut engine = self.engine.lock().await;
        let resp = engine.call(kind).await?;
        drop(engine);

        // Bookkeeping only after the engine accepted the operation.
        if resp.ok {
            if let Some(ipc::response::Kind::Created(c)) = &resp.kind {
                self.owners.lock().await.insert(c.session, self.id);
                self.owned.push(c.session);
                tracing::info!(conn = self.id, session = c.session, "session created");
            }
            if let Some(session) = session
                && closing
            {
                self.owners.lock().await.remove(&session);
                self.owned.retain(|s| *s != session);
                tracing::info!(conn = self.id, session, "session closed");
            }
        }
        Ok(ipc::Response { seq, ..resp })
    }

    /// Close every session this connection created.
    async fn close_owned(&mut self) {
        for session in std::mem::take(&mut self.owned) {
            self.owners.lock().await.remove(&session);
            let mut engine = self.engine.lock().await;
            let r = engine
                .call(ipc::request::Kind::CloseSession(ipc::CloseSession {
                    session,
                }))
                .await;
            if let Err(e) = r {
                tracing::warn!(session, error = %e, "closing orphaned session failed");
            }
        }
    }
}

async fn cleanup_all(engine: &Arc<Mutex<Engine>>, owners: &Owners) {
    let sessions: Vec<u64> = owners.lock().await.keys().copied().collect();
    let mut guard = engine.lock().await;
    for session in sessions {
        let _ = guard
            .call(ipc::request::Kind::CloseSession(ipc::CloseSession {
                session,
            }))
            .await;
    }
}

/// Resolve a path against the daemon's working directory.
///
/// `canonicalize` is not used on purpose: it fails for paths that do not exist
/// yet (the data directory is created by the engine) and it would resolve
/// symlinks behind the user's back.
fn absolute(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path);
    }
    Ok(std::env::current_dir()
        .context("cannot read the working directory")?
        .join(path))
}

/// Pick a socket location that actually works.
///
/// `$XDG_RUNTIME_DIR` is the right place on a normal desktop, but it is not
/// always writable (sandboxes, containers, odd permissions), so probe it and
/// fall back rather than failing to start.
fn default_socket() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        let dir = PathBuf::from(dir);
        if dir_writable(&dir) {
            return dir.join("wetype.sock");
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/state/wetype/wetype.sock");
    }
    std::env::temp_dir().join("wetype.sock")
}

fn dir_writable(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    let probe = dir.join(format!(".wetype-probe-{}", std::process::id()));
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

async fn bind(path: &Path) -> Result<UnixListener> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    // A stale socket from a crashed run may be removed, but never delete a
    // regular file or follow a symlink at the configured path.
    #[cfg(unix)]
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            match tokio::net::UnixStream::connect(path).await {
                Ok(_) => bail!("another daemon is already listening on {}", path.display()),
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                    // Recheck the type before unlinking after the async probe.
                    let still_socket = std::fs::symlink_metadata(path)
                        .map(|metadata| metadata.file_type().is_socket())
                        .unwrap_or(false);
                    if !still_socket {
                        bail!("socket path changed while checking {}", path.display());
                    }
                    std::fs::remove_file(path).with_context(|| {
                        format!("cannot remove stale socket {}", path.display())
                    })?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("cannot check socket {}", path.display()));
                }
            }
        }
        Ok(_) => bail!("refusing to replace non-socket path {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("cannot inspect socket {}", path.display()));
        }
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("cannot bind {}", path.display()))?;
    // Only the owning user may talk to the daemon.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(listener)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[tokio::test]
    async fn bind_refuses_to_replace_a_regular_file() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after UNIX epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("wetype-bind-{}-{nonce}", std::process::id()));
        std::fs::write(&path, b"preserve").expect("create regular-file fixture");

        let result = bind(&path).await;

        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).expect("fixture remains"), b"preserve");
        std::fs::remove_file(path).expect("remove regular-file fixture");
    }
}

//! Blocking façade over the daemon's request/response protocol.
//!
//! The user-facing API is synchronous - fcitx5's `keyEvent` has to answer "do I
//! take this key" before it returns - but the socket is not read on the caller's
//! thread. One supervisor thread owns the connection:
//!
//! * it reads replies and routes them to the waiting caller by `seq`;
//! * it notices a dropped socket, closes it, and retries every
//!   [`DEFAULT_RETRY_INTERVAL`] until the daemon is back - no backoff, no
//!   circuit breaker, and no waiting inside `keyEvent`;
//! * it bumps a generation counter on every successful connect, which is how
//!   the frontend learns that its sessions died with the old daemon.
//!
//! The very first connect is attempted synchronously in [`Client::new`], so the
//! addon's startup protocol check does not race the supervisor thread.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::BytesMut;
use prost::Message;
use wetype_types::codec::{decode_frame, encode_frame, ipc_codec};
use wetype_types::ipc;

/// Default deadline for one request. A warm engine answers in well under a
/// millisecond; this is the "something is wrong" threshold, not a target.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(250);

/// How long to wait between connection attempts while the daemon is away.
pub const DEFAULT_RETRY_INTERVAL: Duration = Duration::from_secs(3);

#[derive(Debug)]
pub enum Error {
    /// No connection to the daemon (never connected, or it went away).
    Disconnected(String),
    /// The daemon did not answer within the deadline.
    Timeout(Duration),
    /// The daemon answered, but refused the request.
    Remote(String),
    /// The reply could not be decoded or did not match the request.
    Protocol(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Disconnected(m) => write!(f, "daemon unavailable: {m}"),
            Error::Timeout(d) => write!(f, "daemon did not answer within {} ms", d.as_millis()),
            Error::Remote(m) => write!(f, "{m}"),
            Error::Protocol(m) => write!(f, "protocol error: {m}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// Everything the supervisor and the callers share.
struct Shared {
    /// Write half of the live connection. `None` means "not connected"; the
    /// supervisor is the only one that puts a stream back in.
    writer: Mutex<Option<UnixStream>>,
    /// Replies are routed by `seq`; a dropped sender wakes its waiter with
    /// `Disconnected`.
    pending: Mutex<HashMap<u64, Sender<ipc::Response>>>,
    next_seq: AtomicU64,
    /// Bumped on every successful connect. Frontends use it to notice that a
    /// daemon restart invalidated every session id they hold.
    generation: AtomicU64,
    /// Set when the client is dropped, so the supervisor stops and does not
    /// log a disconnect that is really a shutdown.
    stopping: AtomicBool,
    /// Whether a socket is connected right now. Mirrors `writer.is_some()`, but
    /// readable without taking the lock - frontends poll it to show the
    /// "service missing" hint.
    connected: AtomicBool,
    timeout_ms: AtomicU64,
    retry_ms: AtomicU64,
}

/// One connection to `wetype-daemon`.
pub struct Client {
    path: String,
    shared: Arc<Shared>,
    shutdown: Option<Sender<()>>,
    supervisor: Option<std::thread::JoinHandle<()>>,
}

impl Client {
    /// Create a client. The first connection attempt happens right here; if the
    /// daemon is not up yet the client is still usable and the supervisor keeps
    /// retrying in the background.
    pub fn new(path: impl Into<String>) -> Self {
        Self::with_retry_interval(path, DEFAULT_RETRY_INTERVAL)
    }

    /// [`Client::new`] with a custom reconnect cadence.
    pub fn with_retry_interval(path: impl Into<String>, retry: Duration) -> Self {
        let path = path.into();
        let shared = Arc::new(Shared {
            writer: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            next_seq: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            stopping: AtomicBool::new(false),
            connected: AtomicBool::new(false),
            timeout_ms: AtomicU64::new(DEFAULT_TIMEOUT.as_millis() as u64),
            retry_ms: AtomicU64::new(retry.as_millis() as u64),
        });

        // Connect once on this thread. The addon pings the daemon immediately
        // after constructing the client; without this it could lose the race
        // against the supervisor and report a bogus "daemon missing".
        let (writer, reader) = match UnixStream::connect(&path) {
            Ok(stream) => match stream.try_clone() {
                Ok(reader) => {
                    let _ = stream.set_write_timeout(Some(DEFAULT_TIMEOUT));
                    (Some(stream), Some(reader))
                }
                Err(_) => (None, None),
            },
            Err(_) => (None, None),
        };
        if writer.is_some() {
            *lock(&shared.writer) = writer;
            shared.connected.store(true, Ordering::SeqCst);
            shared.generation.fetch_add(1, Ordering::SeqCst);
        }

        let (shutdown, stop) = channel();
        let supervisor = {
            let shared = Arc::clone(&shared);
            let path = path.clone();
            std::thread::Builder::new()
                .name("wetype-client".into())
                .spawn(move || supervise(shared, path, stop, reader))
                .ok()
        };

        Self {
            path,
            shared,
            shutdown: Some(shutdown),
            supervisor,
        }
    }

    pub fn with_timeout(self, timeout: Duration) -> Self {
        self.shared
            .timeout_ms
            .store(timeout.as_millis() as u64, Ordering::Relaxed);
        self
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// Incremented on every successful connect. A value different from the one
    /// a session was created on means that session no longer exists.
    pub fn generation(&self) -> u64 {
        self.shared.generation.load(Ordering::SeqCst)
    }

    /// Whether the daemon socket is connected right now. Cheap enough to poll
    /// from a key handler; it never blocks on the reconnect loop.
    pub fn connected(&self) -> bool {
        self.shared.connected.load(Ordering::SeqCst)
    }

    /// Send a request and wait for its reply.
    pub fn call(&self, kind: ipc::request::Kind) -> Result<ipc::Response> {
        let timeout = Duration::from_millis(self.shared.timeout_ms.load(Ordering::Relaxed));
        let seq = self.shared.next_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = channel::<ipc::Response>();
        lock(&self.shared.pending).insert(seq, tx);

        let req = ipc::Request {
            seq,
            kind: Some(kind),
        };
        let mut buf = BytesMut::new();
        if let Err(e) = encode_frame(&req, &mut buf) {
            lock(&self.shared.pending).remove(&seq);
            return Err(Error::Protocol(format!("encode: {e}")));
        }

        if let Err(e) = self.write(&buf) {
            lock(&self.shared.pending).remove(&seq);
            return Err(e);
        }

        match rx.recv_timeout(timeout) {
            Ok(resp) => Ok(resp),
            Err(RecvTimeoutError::Timeout) => {
                lock(&self.shared.pending).remove(&seq);
                Err(Error::Timeout(timeout))
            }
            Err(RecvTimeoutError::Disconnected) => {
                Err(Error::Disconnected("connection lost while waiting".into()))
            }
        }
    }

    fn write(&self, buf: &[u8]) -> Result<()> {
        let mut writer = lock(&self.shared.writer);
        let Some(stream) = writer.as_mut() else {
            return Err(Error::Disconnected("not connected".into()));
        };
        if let Err(e) = stream.write_all(buf).and_then(|()| stream.flush()) {
            // The socket is gone. Shut it down so the supervisor's blocked read
            // returns immediately and it can start reconnecting; dropping the
            // write half alone would leave the read half open.
            let _ = stream.shutdown(Shutdown::Both);
            *writer = None;
            self.shared.connected.store(false, Ordering::SeqCst);
            return Err(Error::Disconnected(e.to_string()));
        }
        Ok(())
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.shared.stopping.store(true, Ordering::SeqCst);
        // Wake the supervisor whether it is waiting for a retry or blocked in
        // `read`.
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(stream) = lock(&self.shared.writer).as_ref() {
            let _ = stream.shutdown(Shutdown::Both);
        }
        self.shared.connected.store(false, Ordering::SeqCst);
        if let Some(handle) = self.supervisor.take() {
            let _ = handle.join();
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Owns the connection for the lifetime of the client.
fn supervise(
    shared: Arc<Shared>,
    path: String,
    stop: std::sync::mpsc::Receiver<()>,
    initial: Option<UnixStream>,
) {
    // The initial stream was connected by `Client::new`, which already
    // installed the writer and bumped the generation.
    let mut reader = initial;

    loop {
        if shared.stopping.load(Ordering::SeqCst) {
            break;
        }

        let connected = match reader.take() {
            Some(reader) => Some(reader),
            None => connect(&shared, &path),
        };
        let Some(reader) = connected else {
            if wait(&shared, &stop) {
                break;
            }
            continue;
        };

        eprintln!(
            "wetype: connected to {path} (generation {})",
            shared.generation.load(Ordering::SeqCst)
        );
        read_replies(reader, &shared);

        *lock(&shared.writer) = None;
        shared.connected.store(false, Ordering::SeqCst);
        lock(&shared.pending).clear();

        if shared.stopping.load(Ordering::SeqCst) {
            break;
        }
        eprintln!(
            "wetype: daemon connection lost, retrying every {} ms",
            shared.retry_ms.load(Ordering::Relaxed)
        );
        if wait(&shared, &stop) {
            break;
        }
    }

    *lock(&shared.writer) = None;
    shared.connected.store(false, Ordering::SeqCst);
    lock(&shared.pending).clear();
}

/// Try to connect once. Returns the read half on success.
fn connect(shared: &Shared, path: &str) -> Option<UnixStream> {
    let stream = UnixStream::connect(path).ok()?;
    let timeout = Duration::from_millis(shared.timeout_ms.load(Ordering::Relaxed));
    let _ = stream.set_write_timeout(Some(timeout));
    let reader = stream.try_clone().ok()?;
    *lock(&shared.writer) = Some(stream);
    shared.connected.store(true, Ordering::SeqCst);
    shared.generation.fetch_add(1, Ordering::SeqCst);
    Some(reader)
}

/// Wait one retry interval. Returns true when the client is shutting down.
fn wait(shared: &Shared, stop: &std::sync::mpsc::Receiver<()>) -> bool {
    let retry = Duration::from_millis(shared.retry_ms.load(Ordering::Relaxed));
    !matches!(stop.recv_timeout(retry), Err(RecvTimeoutError::Timeout))
}

/// Read replies until the socket closes.
fn read_replies(mut stream: UnixStream, shared: &Shared) {
    let mut codec = ipc_codec();
    let mut buf = BytesMut::new();
    loop {
        match decode_frame(&mut buf, &mut codec) {
            Ok(Some(frame)) => {
                let Ok(resp) = ipc::Response::decode(frame) else {
                    continue;
                };
                let waiter = lock(&shared.pending).remove(&resp.seq);
                // An unsolicited frame has no waiter. That is the slot pushed
                // cloud candidates will use; today the protocol never produces
                // one.
                if let Some(tx) = waiter {
                    let _ = tx.send(resp);
                }
            }
            Ok(None) => {
                let mut tmp = [0u8; 4096];
                match stream.read(&mut tmp) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => buf.extend_from_slice(&tmp[..n]),
                }
            }
            Err(_) => return,
        }
    }
}

// ---------------------------------------------------------------- typed calls

impl Client {
    pub fn ping(&self, protocol_version: u32) -> Result<ipc::Pong> {
        let resp = self.call(ipc::request::Kind::Ping(ipc::Ping { protocol_version }))?;
        match resp.kind {
            Some(ipc::response::Kind::Pong(p)) => Ok(p),
            _ => Err(remote_error(resp)),
        }
    }

    pub fn new_session(
        &self,
        ctx_id: u32,
        app_name: &str,
        smart: &[String],
        fuzzy: &[String],
    ) -> Result<u64> {
        let resp = self.call(ipc::request::Kind::NewSession(ipc::NewSession {
            ctx_id,
            app_name: app_name.to_string(),
            smart: smart.to_vec(),
            fuzzy: fuzzy.to_vec(),
        }))?;
        match resp.kind {
            Some(ipc::response::Kind::Created(c)) => Ok(c.session),
            _ => Err(remote_error(resp)),
        }
    }

    pub fn close_session(&self, session: u64) -> Result<()> {
        self.expect_ack(ipc::request::Kind::CloseSession(ipc::CloseSession {
            session,
        }))
    }

    pub fn reset(&self, session: u64) -> Result<()> {
        self.expect_ack(ipc::request::Kind::Reset(ipc::Reset { session }))
    }

    /// Send one printable ASCII key.
    pub fn key(&self, session: u64, key: u8) -> Result<ipc::Candidates> {
        let resp = self.call(ipc::request::Kind::Key(ipc::Key {
            session,
            utf8: vec![key],
        }))?;
        match resp.kind {
            Some(ipc::response::Kind::Candidates(c)) => Ok(c),
            _ => Err(remote_error(resp)),
        }
    }

    pub fn backspace(&self, session: u64, count: u32) -> Result<ipc::Candidates> {
        let resp = self.call(ipc::request::Kind::Backspace(ipc::Backspace {
            session,
            count,
        }))?;
        match resp.kind {
            Some(ipc::response::Kind::Candidates(c)) => Ok(c),
            _ => Err(remote_error(resp)),
        }
    }

    pub fn select(&self, session: u64, generation: u64, index: u32) -> Result<ipc::SelectResult> {
        let resp = self.call(ipc::request::Kind::Select(ipc::Select {
            session,
            generation,
            index,
        }))?;
        match resp.kind {
            Some(ipc::response::Kind::SelectResult(r)) => Ok(r),
            _ => Err(remote_error(resp)),
        }
    }

    fn expect_ack(&self, kind: ipc::request::Kind) -> Result<()> {
        let resp = self.call(kind)?;
        match resp.kind {
            Some(ipc::response::Kind::Ack(_)) => Ok(()),
            _ => Err(remote_error(resp)),
        }
    }
}

fn remote_error(resp: ipc::Response) -> Error {
    if resp.ok {
        Error::Protocol("unexpected reply kind".into())
    } else {
        Error::Remote(resp.error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::time::Instant;

    /// A fake daemon that answers every request with a canned `Pong`.
    ///
    /// Dropping it closes the accepted socket, which is what the reconnect
    /// tests need: dropping a bare `JoinHandle` only detaches the thread, and
    /// the socket would stay open.
    struct FakeDaemon {
        socket: String,
        live: Arc<Mutex<Option<UnixStream>>>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Drop for FakeDaemon {
        fn drop(&mut self) {
            if let Some(stream) = lock(&self.live).take() {
                let _ = stream.shutdown(Shutdown::Both);
            } else {
                // The thread is still blocked in `accept`; a connection makes
                // it return, and the accepted stream is closed at once.
                let _ = UnixStream::connect(&self.socket);
            }
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
            let _ = std::fs::remove_file(&self.socket);
        }
    }

    fn fake_daemon(socket: String) -> FakeDaemon {
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).expect("bind");
        let live = Arc::new(Mutex::new(None));
        let accepted = Arc::clone(&live);
        let thread = std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            // Keep a handle the test can shut down from the outside; shutdown
            // applies to the socket, not to one file descriptor.
            if let Ok(clone) = stream.try_clone() {
                *lock(&accepted) = Some(clone);
            }
            let mut codec = ipc_codec();
            let mut buf = BytesMut::new();
            loop {
                match decode_frame(&mut buf, &mut codec) {
                    Ok(Some(frame)) => {
                        let Ok(req) = ipc::Request::decode(frame) else {
                            break;
                        };
                        let resp = ipc::Response {
                            seq: req.seq,
                            ok: true,
                            error: String::new(),
                            kind: Some(ipc::response::Kind::Pong(ipc::Pong {
                                protocol_version: 1,
                                engine_version: "fake".into(),
                            })),
                        };
                        let mut out = BytesMut::new();
                        encode_frame(&resp, &mut out).expect("encode");
                        if stream.write_all(&out).is_err() {
                            break;
                        }
                    }
                    Ok(None) => {
                        let mut tmp = [0u8; 4096];
                        match stream.read(&mut tmp) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        FakeDaemon {
            socket,
            live,
            thread: Some(thread),
        }
    }

    fn socket_path(tag: &str) -> String {
        format!("/tmp/wetype-test-{}-{tag}.sock", std::process::id())
    }

    #[test]
    fn round_trips_a_reply() {
        let path = socket_path("ok");
        let server = fake_daemon(path.clone());
        let client = Client::new(path.clone());
        let pong = client.ping(1).expect("ping");
        assert_eq!(pong.engine_version, "fake");
        assert!(client.connected(), "a live daemon must report connected");
        drop(server);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_missing_daemon_fails_fast_instead_of_blocking() {
        let path = socket_path("missing");
        let _ = std::fs::remove_file(&path);
        let client = Client::new(path);
        let started = Instant::now();
        assert!(matches!(client.ping(1), Err(Error::Disconnected(_))));
        assert!(!client.connected());
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "connecting to nothing must not wait for the request timeout"
        );
    }

    #[test]
    fn reconnects_on_its_own_when_the_daemon_appears() {
        let path = socket_path("reconnect");
        let _ = std::fs::remove_file(&path);
        let client = Client::with_retry_interval(path.clone(), Duration::from_millis(20));
        assert!(client.ping(1).is_err());
        assert_eq!(client.generation(), 0);

        // The daemon shows up only after the client is already running.
        let server = fake_daemon(path.clone());
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if client.ping(1).is_ok() {
                break;
            }
            assert!(Instant::now() < deadline, "client never reconnected");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(client.generation() >= 1);
        drop(server);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_lost_connection_is_noticed_and_replaced() {
        let path = socket_path("lost");
        let _ = std::fs::remove_file(&path);
        let server = fake_daemon(path.clone());
        let client = Client::with_retry_interval(path.clone(), Duration::from_millis(20));
        client.ping(1).expect("initial ping");
        let first = client.generation();

        // Dropping the server closes the connection; the supervisor must fail
        // the next call and rebuild the connection once a new daemon appears.
        drop(server);
        let deadline = Instant::now() + Duration::from_secs(2);
        while client.ping(1).is_ok() {
            assert!(Instant::now() < deadline, "closed socket still answered");
            std::thread::sleep(Duration::from_millis(10));
        }

        let server = fake_daemon(path.clone());
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if client.ping(1).is_ok() {
                break;
            }
            assert!(Instant::now() < deadline, "client never reconnected");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(client.generation() > first);
        assert!(client.connected());
        drop(server);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_silent_daemon_times_out() {
        let path = socket_path("silent");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind");
        // Accept but never answer.
        std::thread::spawn(move || {
            // Keep the stream alive: dropping it would close the socket and the
            // client would see EPIPE instead of timing out.
            let (_stream, _) = listener.accept().expect("accept");
            std::thread::sleep(Duration::from_secs(5));
        });
        let client = Client::new(path.clone()).with_timeout(Duration::from_millis(120));
        let started = Instant::now();
        let err = client.ping(1).expect_err("must time out");
        assert!(matches!(err, Error::Timeout(_)), "got {err:?}");
        assert!(started.elapsed() < Duration::from_secs(1));
        let _ = std::fs::remove_file(path);
    }
}

//! Handle to the `wetype-engine.exe` child process.
//!
//! `wetype-engine.exe` speaks exactly one request/response stream and is
//! single-threaded by design (its session listener is synchronous), so calls
//! are serialized behind a mutex rather than multiplexed. That is fine: an
//! engine call is tens of microseconds, and typing is the only latency that
//! matters.

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use bytes::BytesMut;
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use wetype_types::codec::ipc_codec;
use wetype_types::ipc;

/// Deadline for one engine call.
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub struct Engine {
    child: Child,
    /// `None` once shutdown or a failed call has closed the pipe.
    stdin: Option<FramedWrite<ChildStdin, LengthDelimitedCodec>>,
    stdout: FramedRead<ChildStdout, LengthDelimitedCodec>,
    seq: u64,
    poisoned: bool,
}

impl Engine {
    /// Spawn the engine host under `wine` (unless `WETYPE_NO_WINE` is set).
    ///
    /// The engine host must live in the same directory as `libwxhld.dll`:
    /// Windows resolves DLLs from the executable's own directory first, and
    /// that is the only reason `wetype-engine.exe` is deployed into the payload
    /// directory rather than run from `target/`.
    pub fn spawn(engine_exe: PathBuf, engine_dir: PathBuf, data_dir: PathBuf) -> Result<Self> {
        let no_wine = std::env::var_os("WETYPE_NO_WINE").is_some();
        if !no_wine {
            // Wine chdir()s into WINEPREFIX before it creates anything, so a
            // custom prefix that does not exist yet makes the launcher exit
            // before the engine host ever runs. The unit points WINEPREFIX at
            // %S/wetype/wineprefix, which nothing has created on a fresh
            // install; create it (and its parents) here.
            if let Some(prefix) = std::env::var_os("WINEPREFIX").filter(|p| !p.is_empty()) {
                std::fs::create_dir_all(&prefix).with_context(|| {
                    format!("cannot create WINEPREFIX {}", PathBuf::from(&prefix).display())
                })?;
            }
        }

        let mut cmd = if no_wine {
            Command::new(&engine_exe)
        } else {
            let mut c = Command::new("wine");
            c.arg(&engine_exe);
            c
        };
        let mut child = cmd
            // Relative dictionary paths (`WETYPE_DICT_RELATIVE`) and the
            // engine's own data lookups are resolved against the working
            // directory, so pin it instead of inheriting the daemon's.
            .current_dir(&engine_dir)
            .env("WETYPE_ENGINE_DIR", &engine_dir)
            .env("WETYPE_DATA_DIR", &data_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Engine logs share the daemon's stderr, which under systemd is the
            // journal.
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            // Own process group: a Ctrl+C in the terminal must reach the daemon
            // only. Otherwise the whole group gets SIGINT, Wine's console
            // handler kills the Windows process behind our back, and the clean
            // `wxime_finalize` shutdown never happens.
            .process_group(0)
            .spawn()
            .with_context(|| format!("cannot spawn engine host {}", engine_exe.display()))?;

        let stdin = child.stdin.take().context("engine host has no stdin")?;
        let stdout = child.stdout.take().context("engine host has no stdout")?;
        let codec = ipc_codec();
        Ok(Self {
            child,
            stdin: Some(FramedWrite::new(stdin, codec.clone())),
            stdout: FramedRead::new(stdout, codec),
            seq: 0,
            poisoned: false,
        })
    }

    /// Send one request and wait for its reply.
    pub async fn call(&mut self, kind: ipc::request::Kind) -> Result<ipc::Response> {
        if self.poisoned {
            bail!("engine host connection is unusable after a failed call");
        }
        self.seq += 1;
        let seq = self.seq;
        let req = ipc::Request {
            seq,
            kind: Some(kind),
        };
        // The codec adds the length prefix itself; `encode_frame` would add a
        // second one and the engine would read the length field as a message.
        let mut buf = BytesMut::with_capacity(req.encoded_len());
        req.encode(&mut buf).context("encode request")?;
        let result = async {
            self.stdin
                .as_mut()
                .context("engine host is shutting down")?
                .send(buf.freeze())
                .await
                .context("write to engine host")?;

            // A wedged engine must not hang the daemon forever: every session
            // would block on this mutex.
            let frame = tokio::time::timeout(CALL_TIMEOUT, self.stdout.next())
                .await
                .context("engine host did not answer in time")?
                .context("engine host closed its stdout")?
                .context("read from engine host")?;
            let resp = ipc::Response::decode(frame).context("decode engine reply")?;
            if resp.seq != seq {
                bail!("engine reply out of order: {} != {seq}", resp.seq);
            }
            Ok(resp)
        }
        .await;
        if result.is_err() {
            // Calls use one ordered stream. After any failed call, a late reply
            // could be mistaken for the next request's response, so never reuse
            // this connection.
            self.poisoned = true;
            self.stdin.take();
            self.child.start_kill().ok();
        }
        result
    }

    /// Non-blocking check for an engine that died on its own.
    pub fn exited(&mut self) -> Option<std::process::ExitStatus> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(status),
            _ => None,
        }
    }

    pub fn unusable(&self) -> bool {
        self.poisoned
    }

    /// Ask the engine to shut down and wait for it.
    ///
    /// Dropping stdin is the shutdown signal: the host finishes, calls
    /// `wxime_finalize`, and exits on its own.
    pub async fn shutdown(&mut self) {
        drop(self.stdin.take());
        match tokio::time::timeout(std::time::Duration::from_secs(5), self.child.wait()).await {
            Ok(Ok(status)) => tracing::info!(%status, "engine host exited"),
            Ok(Err(e)) => tracing::warn!(error = %e, "waiting for engine host failed"),
            Err(_) => {
                // Escalation belongs here, in the supervisor, not in the engine
                // host itself: if closing stdin did not stop it, nothing else
                // we can do from inside would be safer.
                tracing::error!("engine host ignored EOF for 5s, killing it");
                // start_kill sends SIGKILL without waiting; wait() below reaps.
                if let Err(e) = self.child.start_kill() {
                    tracing::error!(error = %e, "cannot kill engine host");
                }
                if let Ok(status) = self.child.wait().await {
                    tracing::warn!(%status, "engine host killed");
                }
            }
        }
    }
}

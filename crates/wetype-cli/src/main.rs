//! Debug client for `wetype-engine.exe`.
//!
//! Spawns the engine host (under `wine`) and drives it over the real protocol,
//! so the whole vertical slice - framing, session creation, keystrokes,
//! candidates, selection - is exercised without fcitx5 in the loop.
//!
//! ```text
//! wetype-cli --engine libs/engine/wetype-engine.exe --keys nihao
//! ```

use std::io::{BufReader, Read, Write};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use bytes::BytesMut;
use prost::Message;
use wetype_types::codec::{decode_frame, encode_frame, ipc_codec};
use wetype_types::ipc;

/// Hold one conversation with the engine host.
struct Conn {
    /// Present when the CLI spawned the engine host itself; absent when it is
    /// talking to `wetype-daemon` over a socket.
    child: Option<std::process::Child>,
    codec: tokio_util::codec::LengthDelimitedCodec,
    inbuf: BytesMut,
    reader: Box<dyn Read>,
    writer: Box<dyn Write>,
    seq: u64,
}

impl Conn {
    fn spawn(engine: &str, prefix_wine: bool) -> Result<Self> {
        let mut cmd = if prefix_wine {
            let mut c = Command::new("wine");
            c.arg(engine);
            c
        } else {
            Command::new(engine)
        };
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Engine logs go to stderr; inherit so they stay visible for debugging.
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("cannot spawn {engine}"))?;
        let writer = child.stdin.take().context("no stdin")?;
        let reader = BufReader::new(child.stdout.take().context("no stdout")?);
        Ok(Self {
            child: Some(child),
            codec: ipc_codec(),
            inbuf: BytesMut::new(),
            reader: Box::new(reader),
            writer: Box::new(writer),
            seq: 0,
        })
    }

    /// Talk to a running `wetype-daemon`.
    fn connect(path: &str) -> Result<Self> {
        let stream = std::os::unix::net::UnixStream::connect(path)
            .with_context(|| format!("cannot connect to {path}"))?;
        let reader = stream.try_clone().context("cannot clone socket")?;
        Ok(Self {
            child: None,
            codec: ipc_codec(),
            inbuf: BytesMut::new(),
            reader: Box::new(reader),
            writer: Box::new(stream),
            seq: 0,
        })
    }

    fn call(&mut self, kind: ipc::request::Kind) -> Result<ipc::Response> {
        self.seq += 1;
        let req = ipc::Request {
            seq: self.seq,
            kind: Some(kind),
        };
        let mut buf = BytesMut::new();
        encode_frame(&req, &mut buf).context("encode request")?;
        self.writer.write_all(&buf).context("write request")?;
        self.writer.flush().ok();

        loop {
            if let Some(frame) = decode_frame(&mut self.inbuf, &mut self.codec)? {
                let resp = ipc::Response::decode(frame).context("decode response")?;
                if resp.seq != self.seq {
                    bail!("sequence mismatch: {} != {}", resp.seq, self.seq);
                }
                return Ok(resp);
            }
            let mut tmp = [0u8; 8192];
            let n = self.reader.read(&mut tmp).context("read response")?;
            if n == 0 {
                let how = match &mut self.child {
                    Some(c) => format!(" (exit {:?})", c.try_wait()),
                    None => String::new(),
                };
                bail!("peer closed the connection{how}");
            }
            self.inbuf.extend_from_slice(&tmp[..n]);
        }
    }

    /// Close the pipe and report whether the host ends on its own.
    ///
    /// The host calls `wxime_finalize` and then exits; this is where we find
    /// out whether that is enough or whether it still needs to be killed.
    fn shutdown(mut self) {
        drop(self.writer);
        let Some(child) = self.child.as_mut() else {
            return;
        };
        let _ = &child;
        for _ in 0..40 {
            match child.try_wait() {
                Ok(Some(status)) => {
                    println!("engine host exited on its own: {status}");
                    return;
                }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
                Err(e) => {
                    println!("wait failed: {e}");
                    break;
                }
            }
        }
        println!("engine host did NOT exit within 2s; killing it");
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut engine = None;
    let mut keys = String::from("nihao");
    let mut native = false;
    let mut socket = None;
    let mut no_close = false;
    let mut steal: Option<u64> = None;
    let mut linger: u64 = 0;
    let mut do_reset = false;
    // Which candidate to select. Without it the first non-empty row is picked,
    // which is a whole-word candidate; partial selection needs an explicit one.
    let mut pick_override: Option<usize> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--engine" => engine = args.next(),
            "--keys" => keys = args.next().unwrap_or_default(),
            "--native" => native = true,
            "--socket" => socket = args.next(),
            "--no-close" => no_close = true,
            "--steal" => steal = args.next().and_then(|v| v.parse().ok()),
            "--linger" => linger = args.next().and_then(|v| v.parse().ok()).unwrap_or(0),
            "--reset" => do_reset = true,
            "--pick" => pick_override = args.next().and_then(|v| v.parse().ok()),
            other => bail!("unknown argument {other}"),
        }
    }
    let engine = engine.unwrap_or_else(|| "wetype-engine.exe".to_string());

    // Derive the engine directory from --engine, so `--engine libs/engine/x.exe`
    // does not silently pick up the current directory for DLL and data lookup.
    if std::env::var_os("WETYPE_ENGINE_DIR").is_none()
        && let Some(dir) = std::path::Path::new(&engine).parent()
        && !dir.as_os_str().is_empty()
    {
        // SAFETY: single-threaded setup, before any thread exists.
        unsafe { std::env::set_var("WETYPE_ENGINE_DIR", dir) };
    }

    let mut conn = match &socket {
        Some(path) => Conn::connect(path)?,
        None => Conn::spawn(&engine, !native)?,
    };

    let pong = conn.call(ipc::request::Kind::Ping(ipc::Ping {
        protocol_version: wetype_types::PROTOCOL_VERSION,
    }))?;
    match pong.kind {
        Some(ipc::response::Kind::Pong(p)) => {
            if p.protocol_version != wetype_types::PROTOCOL_VERSION {
                bail!(
                    "protocol mismatch: host {} addon {}",
                    p.protocol_version,
                    wetype_types::PROTOCOL_VERSION
                );
            }
            println!("engine: {}", p.engine_version);
        }
        _ => bail!("ping failed: {}", pong.error),
    }

    // Debug mode: try to operate another connection's session and report what
    // the daemon says. Used to check the ownership rule.
    if let Some(target) = steal {
        let resp = conn.call(ipc::request::Kind::Key(ipc::Key {
            session: target,
            utf8: b"a".to_vec(),
        }))?;
        println!("steal {target}: ok={} error={:?}", resp.ok, resp.error);
        conn.shutdown();
        return Ok(());
    }

    let created = conn.call(ipc::request::Kind::NewSession(ipc::NewSession {
        ctx_id: 1,
        app_name: String::new(),
    }))?;
    let session = match created.kind {
        Some(ipc::response::Kind::Created(c)) => c.session,
        _ => bail!("new_session failed: {}", created.error),
    };
    println!("session: {session}");

    let mut last = None;
    for ch in keys.chars() {
        // '\b' in --keys means backspace, so a single string can exercise
        // editing without a second flag.
        let resp = if ch == '\u{8}' {
            conn.call(ipc::request::Kind::Backspace(ipc::Backspace {
                session,
                count: 1,
            }))?
        } else {
            conn.call(ipc::request::Kind::Key(ipc::Key {
                session,
                utf8: ch.to_string().into_bytes(),
            }))?
        };
        match resp.kind {
            Some(ipc::response::Kind::Candidates(c)) => {
                println!(
                    "key {ch:?} -> gen {} : {} candidates, preedit {:?} raw {:?} (cursor {})",
                    c.generation,
                    c.items.len(),
                    String::from_utf8_lossy(&c.preedit),
                    String::from_utf8_lossy(&c.raw),
                    c.cursor
                );
                if !c.commit.is_empty() {
                    println!(
                        "   engine committed {:?}",
                        String::from_utf8_lossy(&c.commit)
                    );
                }
                last = Some(c);
            }
            other => println!(
                "key {ch:?} -> {}",
                match other {
                    Some(ipc::response::Kind::Ack(_)) => "ack".to_string(),
                    _ => resp.error.clone(),
                }
            ),
        }
    }

    let Some(last) = last else {
        bail!("no candidates produced");
    };
    if last.items.is_empty() {
        // Legitimate: punctuation and auto-commit produce a commit with no
        // candidates at all.
        println!("no candidates; engine commit only");
        if !last.commit.is_empty() {
            println!("committed {:?}", String::from_utf8_lossy(&last.commit));
        }
        if !no_close {
            let _ = conn.call(ipc::request::Kind::CloseSession(ipc::CloseSession {
                session,
            }))?;
        }
        conn.shutdown();
        return Ok(());
    }
    println!("--- candidates ---");
    // An empty row is never selectable; the engine throws on it.
    let pick = pick_override
        .filter(|i| *i < last.items.len() && !last.items[*i].text.is_empty())
        .or_else(|| last.items.iter().position(|c| !c.text.is_empty()))
        .unwrap_or(0);
    for (i, c) in last.items.iter().enumerate() {
        println!(
            "{i:>2}. {}{} (cover {})",
            String::from_utf8_lossy(&c.text),
            if c.cloud { " [cloud]" } else { "" },
            c.cover
        );
    }

    let picked = conn.call(ipc::request::Kind::Select(ipc::Select {
        session,
        generation: last.generation,
        index: pick as u32,
    }))?;
    match picked.kind {
        Some(ipc::response::Kind::SelectResult(r)) => {
            println!(
                "committed {:?} (finished {})",
                String::from_utf8_lossy(&r.commit),
                r.finished
            );
            if let Some(rest) = r.rest {
                println!(
                    "  still composing: preedit {:?} raw {:?}, {} candidates",
                    String::from_utf8_lossy(&rest.preedit),
                    String::from_utf8_lossy(&rest.raw),
                    rest.items.len()
                );
            }
        }
        _ => bail!("select failed: {}", picked.error),
    }

    if do_reset {
        let r = conn.call(ipc::request::Kind::Reset(ipc::Reset { session }))?;
        println!("reset: ok={} error={:?}", r.ok, r.error);
        // The session must still be usable afterwards; this is where a daemon
        // that mistook Reset's Ack for a close used to break.
        let r = conn.call(ipc::request::Kind::Key(ipc::Key {
            session,
            utf8: b"a".to_vec(),
        }))?;
        println!("key-after-reset: ok={} error={:?}", r.ok, r.error);
    }

    if linger > 0 {
        println!("lingering {linger}s");
        std::thread::sleep(std::time::Duration::from_secs(linger));
    }
    if no_close {
        println!("leaving session {session} open");
    } else {
        let _ = conn.call(ipc::request::Kind::CloseSession(ipc::CloseSession {
            session,
        }))?;
    }
    conn.shutdown();
    Ok(())
}

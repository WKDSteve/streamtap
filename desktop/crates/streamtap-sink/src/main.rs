//! streamtap-sink: spike 2.
//!
//! `serve`  binds a WebSocket server to 127.0.0.1, authenticates each
//!          connection with a shared token and writes every received media
//!          segment to disk, one file per `SourceBuffer` (and per init-segment
//!          "part"), plus an `index.jsonl` with the segment metadata.
//! `mux`    turns a capture directory into a playable MP4 with `ffmpeg -c copy`.
//! `devices` asks a running `serve --tailnet` for a phone's pairing link or the paired phones, or revokes
//!          one (tailnet.rs: phones reach the sink through Tailscale Serve, each with a key of its own).
//! `replay` turns a recording saved on a phone (the extension's target "file on the device") into a
//!          capture directory, through the same session code as `serve`.
//!
//! Wire format (every WebSocket binary message):
//!   u32 little-endian header length | JSON header | payload bytes
//! The first message of a connection is a `hello` header carrying the token.

#![allow(
    clippy::wildcard_imports,
    reason = "the modules of one program, each reading the others' items as one namespace"
)]

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, Read, Seek, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod error;
mod init;
mod mux;
mod proc;
mod replay;
mod serve;
mod session;
mod tailnet;
#[cfg(test)]
mod tests;
mod wire;

use error::SinkError;
use init::*;
use mux::*;
use proc::{Spec, Tool, Tools};
use replay::*;
use serve::*;
use session::*;
use tailnet::*;
use wire::*;

/// Deadline of `ffmpeg -version`.
const VERSION_TIMEOUT: Duration = Duration::from_secs(20);
/// Deadline of one ffprobe run on a part.
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);
/// Default deadline of the mux (`--timeout-secs`): a stream copy of a
/// 16 GiB capture takes minutes, not hours.
const DEFAULT_MUX_TIMEOUT_SECS: u64 = 3600;

const MAX_HEADER: usize = 64 * 1024;
const MAX_MESSAGE: usize = 64 * 1024 * 1024;
/// `SourceBuffers` per session; a page can forge records with arbitrary ids,
/// and each track holds an open file.
const MAX_TRACKS: usize = 32;
/// Refused tracks remembered across connections (oldest forgotten first).
const MAX_REFUSALS: usize = 4096;
/// Characters of the extension's session id the sink keys on.
const MAX_SESSION_ID: usize = 64;
/// Malformed messages tolerated per connection after `hello` before closing.
const MAX_BAD_MESSAGES: u32 = 100;
/// Default cap of bytes written per session (`--max-session-gib`).
const DEFAULT_MAX_SESSION_GIB: u64 = 16;
/// Default cap of bytes written by one sink process (`--max-total-gib`).
const DEFAULT_MAX_TOTAL_GIB: u64 = 64;
const DEFAULT_PORT: u16 = 17_650;
/// Default time a connection has from accept to a valid `hello` (the TCP
/// accept, the WebSocket handshake and the first message together). A client
/// that never finishes would hold its thread for good (`--hello-timeout-secs`).
const DEFAULT_HELLO_TIMEOUT_SECS: u64 = 10;
/// Default silence tolerated after `hello` (`--idle-timeout-secs`). Generous: a
/// paused video sends nothing for minutes, and that must not end the capture.
const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 3600;
// A paused video must not end the capture: the default tolerates well over ten minutes of silence.
const _: () = assert!(DEFAULT_IDLE_TIMEOUT_SECS >= 1800);
/// Default cap of simultaneous connections, one thread each (`--max-connections`).
/// More are closed at once, before a thread is started.
const DEFAULT_MAX_CONNECTIONS: usize = 16;
/// Connections waiting for their `hello` at the same time. One more pushes the oldest of them
/// out, so whoever connects and says hello at once always gets in, however many silent
/// connections a local process keeps opening. Far below `DEFAULT_MAX_CONNECTIONS`, so they cannot
/// use up the places either.
const MAX_WAITING_FOR_HELLO: usize = 4;
/// How soon a connection that was pushed out notices it and frees its place.
const WAITING_POLL: Duration = Duration::from_millis(50);
/// How long a write to a client that does not read may block.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// Origin of the streamtap extension. Its id is fixed by the `key` in
/// `extension/manifest.json`; the service worker owns the socket, so the
/// handshake carries exactly this origin (SRV-03/04: compared exactly).
const DEFAULT_ORIGIN: &str = "chrome-extension://oggpddakdbffbjhkjmcakidgmhpgakia";

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, result) = match args.first().map(String::as_str) {
        Some("serve") => ("serve", run_serve(&args[1..])),
        Some("mux") => ("mux", run_mux(&args[1..], &Tools::default())),
        Some("replay") => ("replay", run_replay(&args[1..])),
        Some("devices") => ("devices", run_devices(&args[1..])),
        _ => (
            "streamtap-sink",
            Err(SinkError::Usage(format!(
                "usage:\n  streamtap-sink serve (--token-stdin | --token <token>) [--port {DEFAULT_PORT}] [--out captures] [--allow-origin {DEFAULT_ORIGIN}] [--hello-timeout-secs {DEFAULT_HELLO_TIMEOUT_SECS}] [--idle-timeout-secs {DEFAULT_IDLE_TIMEOUT_SECS}] [--max-connections {DEFAULT_MAX_CONNECTIONS}] [--tailnet <pc.example.ts.net> [--devices <devices.json>] [--tailnet-session-hours {DEFAULT_TAILNET_SESSION_HOURS}]]\n  streamtap-sink devices [pair | revoke <id>] (--token-stdin | --token <token>) [--port {DEFAULT_PORT}]\n  streamtap-sink mux <capture-dir> [--out <file.mp4>] [--timeout-secs {DEFAULT_MUX_TIMEOUT_SECS}] [--verbose]
  streamtap-sink replay <recording.streamtap> [--out captures]"
            ))),
        ),
    };
    let code = match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{cmd}: {e}");
            e.exit_code()
        }
    };
    std::process::exit(code);
}

/// The value after `name`, `None` when the flag is absent. A flag without a
/// value, or followed by another flag (`--token --port 1` would make
/// "--port" the token), is a usage error, not a silent default.
fn flag<'a>(args: &'a [String], name: &str) -> Result<Option<&'a str>, SinkError> {
    let Some(i) = args.iter().position(|a| a == name) else {
        return Ok(None);
    };
    match args.get(i + 1).map(String::as_str) {
        Some(v) if !v.starts_with("--") => Ok(Some(v)),
        _ => Err(SinkError::Usage(format!("{name} needs a value"))),
    }
}

// ---------------------------------------------------------------------------
// serve
// ---------------------------------------------------------------------------

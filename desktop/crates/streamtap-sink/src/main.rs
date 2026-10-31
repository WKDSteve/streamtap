//! streamtap-sink: spike 2.
//!
//! `serve`  binds a WebSocket server to 127.0.0.1, authenticates each
//!          connection with a shared token and writes every received media
//!          segment to disk, one file per `SourceBuffer` (and per init-segment
//!          "part"), plus an `index.jsonl` with the segment metadata.
//! `mux`    turns a capture directory into a playable MP4 with `ffmpeg -c copy`.
//!
//! Wire format (every WebSocket binary message):
//!   u32 little-endian header length | JSON header | payload bytes
//! The first message of a connection is a `hello` header carrying the token.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

mod error;
mod proc;

use error::SinkError;
use proc::{Spec, Tool, Tools};

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
/// Malformed messages tolerated per connection after `hello` before closing.
const MAX_BAD_MESSAGES: u32 = 100;
/// Default cap of bytes written per session (`--max-session-gib`).
const DEFAULT_MAX_SESSION_GIB: u64 = 16;
/// Default cap of bytes written by one sink process (`--max-total-gib`).
const DEFAULT_MAX_TOTAL_GIB: u64 = 64;
const DEFAULT_PORT: u16 = 17_650;
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
        _ => (
            "streamtap-sink",
            Err(SinkError::Usage(format!(
                "usage:\n  streamtap-sink serve --token <token> [--port {DEFAULT_PORT}] [--out captures] [--allow-origin {DEFAULT_ORIGIN}]\n  streamtap-sink mux <capture-dir> [--out <file.mp4>] [--timeout-secs {DEFAULT_MUX_TIMEOUT_SECS}]"
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

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

// ---------------------------------------------------------------------------
// serve
// ---------------------------------------------------------------------------

fn run_serve(args: &[String]) -> Result<(), SinkError> {
    let Some(token) = flag(args, "--token") else {
        return Err(SinkError::Usage(
            "--token is required (the extension must send the same value)".into(),
        ));
    };
    let port = match flag(args, "--port").map(str::parse::<u16>) {
        None => DEFAULT_PORT,
        Some(Ok(p)) => p,
        Some(Err(_)) => return Err(SinkError::Usage("--port must be a number".into())),
    };
    let out = PathBuf::from(flag(args, "--out").unwrap_or("captures"));
    let origin = flag(args, "--allow-origin")
        .unwrap_or(DEFAULT_ORIGIN)
        .to_owned();
    let gib = |name: &str, default: u64| match flag(args, name).map(str::parse::<u64>) {
        None => Ok(default << 30),
        Some(Ok(g)) if (1..=4096).contains(&g) => Ok(g << 30),
        Some(_) => Err(SinkError::Usage(format!(
            "{name} must be a whole number from 1 to 4096"
        ))),
    };
    let per_session = gib("--max-session-gib", DEFAULT_MAX_SESSION_GIB)?;
    let total_max = gib("--max-total-gib", DEFAULT_MAX_TOTAL_GIB)?;
    let budget = Arc::new(Budget {
        per_session,
        total_max,
        total_used: AtomicU64::new(0),
    });
    fs::create_dir_all(&out).map_err(|e| SinkError::io("cannot create", Some(out.clone()), e))?;

    // Loopback only. Binding anything else is not an option, not a flag.
    let listener = TcpListener::bind(("127.0.0.1", port))
        .map_err(|e| SinkError::io(format!("bind 127.0.0.1:{port} failed"), None, e))?;
    println!(
        "streamtap-sink listening on ws://127.0.0.1:{port}  out={}  origin={origin}",
        out.display()
    );

    let token = token.as_bytes().to_vec();
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let token = token.clone();
                let out = out.clone();
                let origin = origin.clone();
                let budget = Arc::clone(&budget);
                std::thread::spawn(move || {
                    if let Err(e) = handle_connection(s, &token, &origin, &out, &budget) {
                        eprintln!("connection ended with error: {e}");
                    }
                });
            }
            Err(e) => eprintln!("accept failed: {e}"),
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Header {
    Hello {
        token: String,
        #[serde(default)]
        session: String,
        #[serde(default)]
        url: String,
        #[serde(default)]
        title: String,
    },
    Segment(SegmentHeader),
    Event {
        name: String,
        #[serde(default)]
        sb: Option<u32>,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "wire format: independent flags of one append"
)]
struct SegmentHeader {
    sb: u32,
    seq: u64,
    /// `MediaSource` the `SourceBuffer` belongs to (null for `SourceBuffers` the
    /// hook only saw after they were created).
    #[serde(default)]
    ms: Option<u32>,
    #[serde(default)]
    mime: String,
    #[serde(default, rename = "isInit")]
    is_init: bool,
    /// The rest of an init segment that did not fit the previous append (the
    /// hook saw the init still open).
    #[serde(default, rename = "initCont")]
    init_cont: bool,
    #[serde(default)]
    kind: String,
    /// Decode time of the first fragment, in `timescale` ticks. Above 2^53 the
    /// extension sends it as a decimal string (JS numbers lose precision).
    #[serde(default, deserialize_with = "de_opt_u64")]
    tfdt: Option<u64>,
    #[serde(default)]
    timescale: Option<u32>,
    /// Duration of the append's first track in `timescale` ticks, if known.
    #[serde(default, deserialize_with = "de_opt_u64")]
    dur: Option<u64>,
    /// `SourceBuffer.timestampOffset` in seconds at the time of the append.
    /// Multi-period DASH and HLS discontinuities restart `tfdt` and shift the
    /// timeline with it, so dedupe and ordering must include it.
    #[serde(default)]
    tso: Option<f64>,
    /// The `SourceBuffer` is in "sequence" mode: the browser places media
    /// one after the other and rewrites `timestampOffset` itself.
    #[serde(default, rename = "seqMode")]
    seq_mode: bool,
    /// The player set `timestampOffset` since the previous append. In
    /// sequence mode the browser then starts this append at that offset.
    #[serde(default, rename = "tsoSet")]
    tso_set: bool,
}

/// `Option<u64>` from a JSON number or a decimal string.
fn de_opt_u64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    use serde::de::Error;
    match Option::<serde_json::Value>::deserialize(d)? {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Number(n)) => n
            .as_u64()
            .map(Some)
            .ok_or_else(|| D::Error::custom("not an unsigned 64-bit integer")),
        Some(serde_json::Value::String(s)) if s.len() <= 20 => {
            s.parse::<u64>().map(Some).map_err(D::Error::custom)
        }
        Some(_) => Err(D::Error::custom("expected a number or a decimal string")),
    }
}

#[derive(Debug, Serialize)]
struct IndexLine<'a> {
    sb: u32,
    ms: Option<u32>,
    part: u32,
    seq: u64,
    offset: u64,
    len: usize,
    /// Init bytes: an init segment or the continuation of one.
    is_init: bool,
    tfdt: Option<u64>,
    timescale: Option<u32>,
    dur: Option<u64>,
    tso: Option<f64>,
    seq_mode: bool,
    tso_set: bool,
    mime: &'a str,
    kind: &'a str,
}

/// Media segments remembered for dedupe, per session (all parts and tracks
/// together): beyond this the session takes no more media.
const MAX_SEEN_PER_SESSION: usize = 400_000;
/// Init bytes held until the init is complete (an init split across appends),
/// counting `PENDING_CHUNK_COST` for each chunk's header.
const MAX_PENDING_INIT: usize = 4 * 1024 * 1024;
const PENDING_CHUNK_COST: usize = 512;
/// Part files per `SourceBuffer` (one per distinct init).
const MAX_PARTS_PER_TRACK: u32 = 64;
/// Budget charged for each part file (its directory entry and cluster).
const PART_FILE_COST: u64 = 4096;

/// Dedupe keys: (tfdt, timestampOffset in ms, duration), or in sequence mode
/// (payload hash, `i64::MIN`, length).
type SeenSet = HashSet<(u64, i64, u64)>;

#[allow(
    clippy::struct_excessive_bools,
    reason = "independent per-track flags, not states of one machine"
)]
struct TrackState {
    part: u32,
    file: File,
    offset: u64,
    init_hash: Option<u64>,
    seen: SeenSet,
    /// Offset and dedupe set of the parts that are not current, so a switch
    /// back to an earlier init continues its part.
    saved: HashMap<u32, (u64, SeenSet)>,
    /// Init hash (of the complete init) -> its part.
    hash_part: HashMap<u64, u32>,
    /// Parts created so far.
    parts: u32,
    /// Chunks of an init still arriving (it may come in several appends): the
    /// decision (duplicate, switch back, new part) waits for the whole init.
    /// Header and length of each chunk; the bytes are `pending_whole`.
    pending: Vec<(SegmentHeader, usize)>,
    pending_whole: Vec<u8>,
    /// Where the completeness scan of `pending_whole` resumes, and whether it
    /// found the `moov` / `Tracks` (each byte is scanned once).
    pending_scan: usize,
    pending_done: bool,
    /// The pending init came right after an encrypted stretch.
    pending_after_refusal: bool,
    /// Container of the last init ("mp4", "webm"): continuation chunks have
    /// no kind of their own.
    init_kind: String,
    /// The last timed chunk was a duplicate: the untimed chunks that follow
    /// it (its continuation) are duplicates too.
    skip_tail: bool,
    /// The current init segment is encrypted (EME/DRM): nothing is written
    /// until a clear init segment arrives.
    refused: bool,
    /// A new init came beyond `MAX_PARTS_PER_TRACK`: its media is skipped.
    parts_full: bool,
    bytes: u64,
    segments: u64,
    dupes: u64,
    skipped: u64,
}

/// Disk budget shared by all sessions of one sink process.
struct Budget {
    per_session: u64,
    total_max: u64,
    total_used: AtomicU64,
}

impl Budget {
    /// Charges `n` bytes against the total; false (and nothing charged) when
    /// it would exceed the cap.
    fn charge(&self, n: u64) -> bool {
        let mut used = self.total_used.load(Ordering::Relaxed);
        loop {
            let next = used.saturating_add(n);
            if next > self.total_max {
                return false;
            }
            match self.total_used.compare_exchange_weak(
                used,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(now) => used = now,
            }
        }
    }
}

struct Session {
    dir: PathBuf,
    index: File,
    tracks: HashMap<u32, TrackState>,
    track_limit_logged: bool,
    /// Bytes written in this session (payload and index lines).
    bytes_total: u64,
    budget: Arc<Budget>,
    /// The session or the process hit its budget: nothing more is written.
    stopped: bool,
    /// Dedupe entries of all tracks and parts of this session.
    seen_total: usize,
    seen_limit_logged: bool,
}

/// PIFF sample encryption box (`uuid` A2394F52-5A9B-4F14-A244-6C427C648DF4).
const PIFF_SAMPLE_ENCRYPTION: [u8; 16] = [
    0xa2, 0x39, 0x4f, 0x52, 0x5a, 0x9b, 0x4f, 0x14, 0xa2, 0x44, 0x6c, 0x42, 0x7c, 0x64, 0x8d, 0xf4,
];

/// True when a segment carries protected media. Init bytes (an init or a
/// continuation of one, `init_part`; `init_kind` is the init's container) are
/// scanned for the markers: `encv`/`enca`/`tenc`/`pssh` (MP4), or the
/// `ContentEncodings`/`ContentEncryption`/`ContentEncKeyID` IDs (`WebM`).
/// Media is only read as structure: a top-level `moov` or `pssh`, or `senc` /
/// PIFF sample encryption in a `moof`. Random bytes in media never count.
fn payload_encrypted(h: &SegmentHeader, payload: &[u8], init_part: bool, init_kind: &str) -> bool {
    let webm = h.kind == "webm" || (init_part && !h.is_init && init_kind == "webm");
    if h.is_init || init_part {
        return if webm {
            webm_encrypted(payload)
        } else {
            looks_encrypted(payload)
        };
    }
    if webm {
        return false;
    }
    boxes_in(payload, 0, payload.len())
        .iter()
        .any(|(kind, s, e)| match kind {
            b"moov" => looks_encrypted(&payload[*s..*e]),
            b"pssh" => true,
            b"moof" => {
                let m = &payload[*s..*e];
                m.windows(4).any(|w| w == b"senc")
                    || m.windows(16).any(|w| w == PIFF_SAMPLE_ENCRYPTION)
            }
            _ => false,
        })
}

/// An EBML variable-size integer at `off`: (value, length). `keep_marker`
/// keeps the length marker (element IDs).
fn read_vint(b: &[u8], off: usize, keep_marker: bool) -> Option<(u64, usize)> {
    let first = *b.get(off)?;
    let len = first.leading_zeros() as usize + 1;
    if len > 8 || off + len > b.len() {
        return None;
    }
    let mask = if len == 8 { 0 } else { 0xffu8 >> len };
    let mut v = u64::from(if keep_marker { first } else { first & mask });
    for &byte in &b[off + 1..off + len] {
        v = (v << 8) | u64::from(byte);
    }
    Some((v, len))
}

/// Top-level MP4 boxes that belong to a media segment, not to its init.
const MEDIA_BOXES: [&[u8; 4]; 7] = [
    b"styp", b"sidx", b"ssix", b"emsg", b"prft", b"moof", b"mdat",
];

/// Where the init part of an init+media append ends (video.js VHS appends the
/// init together with the first segment): the first top-level media box
/// (`styp`, `sidx`, `emsg`, `moof`, ...; MP4) or the first Cluster (`WebM`).
/// Per-segment boxes in front of the `moof` (a `sidx` differs per segment)
/// must not count as init bytes, or every combined append would look like a
/// new init. `None` when there is no media in it.
fn init_prefix_len(kind: &str, payload: &[u8]) -> Option<usize> {
    if kind == "webm" {
        let mut off = 0;
        while off < payload.len() {
            let (id, il) = read_vint(payload, off, true)?;
            let (size, sl) = read_vint(payload, off + il, false)?;
            if id == 0x1f43_b675 {
                return (off > 0).then_some(off);
            }
            let body = off + il + sl;
            let unknown = sl < 8 && size == (1u64 << (7 * sl)) - 1;
            if id == 0x1853_8067 || unknown {
                off = body; // descend into Segment
                continue;
            }
            off = body.checked_add(usize::try_from(size).ok()?)?;
        }
        return None;
    }
    // A box starts where the previous one ended (headers may be 8 or 16 bytes).
    // Only with a moof/mdat after it: an init+index range of a single-file
    // stream (ftyp moov sidx) is all init.
    // A last box cut off (a chunked fetch) counts by its type, as in the
    // hook: (type, start) of every box, complete or not.
    let complete = boxes_in(payload, 0, payload.len());
    let mut starts: Vec<([u8; 4], usize)> = Vec::with_capacity(complete.len() + 1);
    let mut at = 0;
    for (k, _, end) in &complete {
        starts.push((*k, at));
        at = *end;
    }
    if let Some(t) = payload.get(at + 4..at + 8) {
        starts.push(([t[0], t[1], t[2], t[3]], at));
    }
    let first_media = starts.iter().position(|(k, _)| MEDIA_BOXES.contains(&k))?;
    if !starts[first_media..]
        .iter()
        .any(|(k, _)| k == b"moof" || k == b"mdat")
    {
        return None;
    }
    let start = starts[first_media].1;
    (start > 0).then_some(start)
}

/// The init bytes so far hold a complete `moov` (MP4) or `Tracks` (`WebM`):
/// nothing more of this init is to come.
#[cfg(test)]
fn init_complete(kind: &str, bytes: &[u8]) -> bool {
    init_scan(kind, bytes, 0).0
}

/// Scans init bytes for a complete `moov` (MP4) or `Tracks` (`WebM`) from
/// `from`, the start of the first box or element not yet complete. Returns
/// whether it was found and where the next scan resumes, so bytes arriving
/// in many chunks are each walked once.
fn init_scan(kind: &str, bytes: &[u8], from: usize) -> (bool, usize) {
    let mut off = from;
    if kind == "webm" {
        while off < bytes.len() {
            let Some((id, il)) = read_vint(bytes, off, true) else {
                return (false, off);
            };
            let Some((size, sl)) = read_vint(bytes, off + il, false) else {
                return (false, off);
            };
            let body = off + il + sl;
            let unknown = sl < 8 && size == (1u64 << (7 * sl)) - 1;
            if id == 0x1853_8067 || unknown {
                off = body; // descend into Segment
                continue;
            }
            let Some(end) = usize::try_from(size).ok().and_then(|s| body.checked_add(s)) else {
                return (false, off);
            };
            if end > bytes.len() {
                return (false, off);
            }
            if id == 0x1654_ae6b {
                return (true, end);
            }
            off = end;
        }
        return (false, off);
    }
    while off + 8 <= bytes.len() {
        let size32 =
            u32::from_be_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]]);
        let (size, header) = match size32 {
            // Up to the end of the stream: never complete before it ends.
            0 => return (false, off),
            1 => {
                let Some(b) = bytes.get(off + 8..off + 16) else {
                    return (false, off);
                };
                let mut v = [0u8; 8];
                v.copy_from_slice(b);
                match usize::try_from(u64::from_be_bytes(v)) {
                    Ok(s) => (s, 16),
                    Err(_) => return (false, off),
                }
            }
            s => (s as usize, 8),
        };
        let Some(end) = off
            .checked_add(size)
            .filter(|e| size >= header && *e <= bytes.len())
        else {
            return (false, off);
        };
        if &bytes[off + 4..off + 8] == b"moov" {
            return (true, end);
        }
        off = end;
    }
    (false, off)
}

/// `WebM`/Matroska encryption in init bytes: the element IDs
/// `ContentEncodings` (0x6D80), `ContentEncryption` (0x5035) and
/// `ContentEncKeyID` (0x47E2) all present (clear data practically never has
/// all three; the extension reads the structure where it can).
fn webm_encrypted(bytes: &[u8]) -> bool {
    let has = |a: u8, b: u8| bytes.windows(2).any(|w| w[0] == a && w[1] == b);
    has(0x6d, 0x80) && has(0x50, 0x35) && has(0x47, 0xe2)
}

/// A page-controlled string for the terminal: printable ASCII only, at most
/// `max` characters (no escape sequences).
fn printable(s: &str, max: usize) -> String {
    s.chars()
        .take(max)
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '?'
            }
        })
        .collect()
}

/// `timestampOffset` in whole milliseconds; non-finite values count as 0.
fn tso_millis(tso: Option<f64>) -> i64 {
    let v = tso.filter(|v| v.is_finite()).unwrap_or(0.0) * 1000.0;
    // Clamped to about ±31 years, so the cast below cannot truncate.
    #[allow(clippy::cast_possible_truncation)]
    let ms = v.clamp(-1e12, 1e12).round() as i64;
    ms
}

/// True when an init segment declares protected tracks (ISO BMFF `encv`,
/// `enca`, `sinf`/`tenc` or a `pssh` box). Such content is DRM: streamtap
/// leaves it alone instead of storing ciphertext.
fn looks_encrypted(init: &[u8]) -> bool {
    const MARKERS: [&[u8; 4]; 4] = [b"encv", b"enca", b"tenc", b"pssh"];
    init.windows(4)
        .any(|w| MARKERS.iter().any(|m| w == m.as_slice()))
}

/// Handshake guard: the `Origin` header must equal `origin` exactly; anything
/// else (missing, a web page, another extension) gets 403 before any file is
/// touched.
// The error type is fixed by tungstenite's handshake `Callback` trait; it
// cannot be boxed.
#[allow(clippy::result_large_err)]
fn check_origin(
    origin: &str,
    req: &tungstenite::handshake::server::Request,
    resp: tungstenite::handshake::server::Response,
) -> Result<tungstenite::handshake::server::Response, tungstenite::handshake::server::ErrorResponse>
{
    let got = req
        .headers()
        .get("origin")
        .map(tungstenite::http::HeaderValue::as_bytes);
    if got == Some(origin.as_bytes()) {
        return Ok(resp);
    }
    // Name the rejected origin (printable ASCII, truncated) so a stale
    // extension or a page is recognisable in the log.
    let shown: String = got.map_or_else(
        || "<none>".to_owned(),
        |b| {
            b.iter()
                .take(120)
                .map(|&c| {
                    if (0x20..0x7f).contains(&c) {
                        char::from(c)
                    } else {
                        '?'
                    }
                })
                .collect()
        },
    );
    eprintln!("origin rejected: {shown}");
    let mut err =
        tungstenite::handshake::server::ErrorResponse::new(Some("origin not allowed".to_owned()));
    *err.status_mut() = tungstenite::http::StatusCode::FORBIDDEN;
    Err(err)
}

/// The next binary message; `None` when the connection ended (closed,
/// broken). Pings are answered, other frame types skipped.
fn next_binary(
    ws: &mut tungstenite::WebSocket<TcpStream>,
    peer: std::net::SocketAddr,
) -> Option<tungstenite::Bytes> {
    loop {
        match ws.read() {
            Ok(tungstenite::Message::Binary(b)) => return Some(b),
            Ok(tungstenite::Message::Close(_)) => {
                // tungstenite queued the close reply; send it so the client
                // sees a clean close instead of a dropped connection.
                let _ = ws.flush();
                return None;
            }
            Ok(tungstenite::Message::Ping(p)) => {
                let _ = ws.send(tungstenite::Message::Pong(p));
            }
            Ok(_) => {}
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                return None
            }
            Err(e) => {
                eprintln!("{peer}: connection error: {e}");
                return None;
            }
        }
    }
}

// The handshake closure returns tungstenite's fixed (large) error type.
#[allow(clippy::result_large_err)]
fn handle_connection(
    stream: TcpStream,
    token: &[u8],
    origin: &str,
    out: &Path,
    budget: &Arc<Budget>,
) -> io::Result<()> {
    let peer = stream.peer_addr()?;
    // tungstenite's default caps a frame at 16 MiB; segments may be up to
    // MAX_MESSAGE, so the frame and message caps follow it (plus header).
    let config = tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE + MAX_HEADER + 4))
        .max_frame_size(Some(MAX_MESSAGE + MAX_HEADER + 4));
    let mut ws = tungstenite::accept_hdr_with_config(
        stream,
        |req: &_, resp| check_origin(origin, req, resp),
        Some(config),
    )
    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{peer}: {e}")))?;
    let mut session: Option<Session> = None;
    let mut bad_messages = 0u32;

    // A broken connection still ends with the summary below.
    while let Some(data) = next_binary(&mut ws, peer) {
        if data.len() > MAX_MESSAGE {
            eprintln!(
                "{peer}: message of {} bytes exceeds limit, closing",
                data.len()
            );
            break;
        }
        let (header, payload) = match split_message(&data) {
            Ok(x) => x,
            // Before hello nothing is trusted: close.
            Err(e) if session.is_none() => {
                eprintln!("{peer}: bad message before hello ({e}), closing");
                break;
            }
            // After hello a malformed record (a page can forge the hook's
            // records) costs that record, not the whole capture.
            Err(e) => {
                bad_messages += 1;
                eprintln!("{peer}: bad message skipped ({bad_messages}): {e}");
                if bad_messages > MAX_BAD_MESSAGES {
                    eprintln!("{peer}: too many bad messages, closing");
                    break;
                }
                continue;
            }
        };

        match (header, session.as_mut()) {
            (
                Header::Hello {
                    token: t,
                    session: id,
                    url,
                    title,
                },
                None,
            ) => {
                if !constant_time_eq(t.as_bytes(), token) {
                    eprintln!("{peer}: wrong token, closing");
                    break;
                }
                // A spent process budget creates no further directories.
                let Some(s) = Session::open(out, &id, &url, &title, Arc::clone(budget))? else {
                    eprintln!("{peer}: --max-total-gib reached, closing");
                    break;
                };
                println!(
                    "{peer}: session {} ({})",
                    s.dir.display(),
                    printable(&url, 300)
                );
                session = Some(s);
            }
            (Header::Hello { .. }, Some(_)) => {
                eprintln!("{peer}: second hello, ignoring");
            }
            (_, None) => {
                eprintln!("{peer}: message before hello, closing");
                break;
            }
            (Header::Segment(h), Some(s)) => {
                if let Err(e) = s.write_segment(&h, payload) {
                    eprintln!("{}: write failed, closing: {e}", s.dir.display());
                    break;
                }
            }
            (Header::Event { name, sb }, Some(s)) => {
                println!("{}: event {name} sb={sb:?}", s.dir.display());
                if let ("initlost", Some(sb)) = (name.as_str(), sb) {
                    if let Err(e) = s.note_lost_init(sb) {
                        eprintln!("{}: write failed, closing: {e}", s.dir.display());
                        break;
                    }
                }
                if name == "endofstream" {
                    s.finish();
                    s.summary();
                }
            }
        }
    }

    if let Some(mut s) = session {
        s.finish();
        s.summary();
    }
    Ok(())
}

fn split_message(data: &[u8]) -> Result<(Header, &[u8]), String> {
    if data.len() < 4 {
        return Err("shorter than header length field".into());
    }
    let hlen = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if hlen > MAX_HEADER || 4 + hlen > data.len() {
        return Err(format!("header length {hlen} out of range"));
    }
    let header: Header = serde_json::from_slice(&data[4..4 + hlen]).map_err(|e| e.to_string())?;
    Ok((header, &data[4 + hlen..]))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn sanitize(s: &str, max: usize) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(max)
        .collect()
}

/// What `TrackState::admit_media` decided for one segment.
#[derive(Debug, PartialEq, Eq)]
enum Admit {
    Skip,
    /// Write it; `as_init` marks init bytes.
    Write {
        as_init: bool,
    },
}

impl TrackState {
    fn new(file: File) -> Self {
        Self {
            part: 0,
            file,
            offset: 0,
            init_hash: None,
            seen: HashSet::new(),
            saved: HashMap::new(),
            hash_part: HashMap::new(),
            parts: 1,
            pending: Vec::new(),
            pending_whole: Vec::new(),
            pending_scan: 0,
            pending_done: false,
            pending_after_refusal: false,
            init_kind: String::new(),
            skip_tail: false,
            refused: false,
            parts_full: false,
            bytes: 0,
            segments: 0,
            dupes: 0,
            skipped: 0,
        }
    }

    /// Holds one more chunk of the pending init.
    fn push_pending(&mut self, h: &SegmentHeader, payload: &[u8]) {
        self.pending.push((h.clone(), payload.len()));
        self.pending_whole.extend_from_slice(payload);
        if !self.pending_done {
            let (done, next) = init_scan(&self.init_kind, &self.pending_whole, self.pending_scan);
            self.pending_done = done;
            self.pending_scan = next;
        }
    }

    /// Memory the pending init holds: its bytes plus each chunk's header
    /// (a page can send it in 1-byte chunks).
    fn pending_cost(&self) -> usize {
        self.pending_whole.len() + self.pending.len() * PENDING_CHUNK_COST
    }

    /// Takes the pending chunks and their bytes, leaving nothing pending.
    fn take_pending(&mut self) -> (Vec<(SegmentHeader, usize)>, Vec<u8>) {
        self.pending_scan = 0;
        self.pending_done = false;
        (
            std::mem::take(&mut self.pending),
            std::mem::take(&mut self.pending_whole),
        )
    }

    /// Makes `part` the current part file, keeping the state of the old one.
    fn switch_part(&mut self, dir: &Path, sb: u32, part: u32) -> io::Result<()> {
        let seen = std::mem::take(&mut self.seen);
        self.saved.insert(self.part, (self.offset, seen));
        let (offset, seen) = self.saved.remove(&part).unwrap_or_default();
        self.file = Session::open_part(dir, sb, part)?;
        self.part = part;
        self.offset = offset;
        self.seen = seen;
        Ok(())
    }

    /// Decides about a complete init (hash over all its chunks): a duplicate
    /// or a switch back to an earlier part writes nothing; a new init writes
    /// into a new part (or the first, still empty one).
    fn decide_init(&mut self, hash: u64, dir: &Path, sb: u32) -> io::Result<bool> {
        self.skip_tail = false;
        let after_refusal = std::mem::take(&mut self.pending_after_refusal);
        if self.init_hash == Some(hash) && !after_refusal {
            // Same init again (player re-appends after a seek).
            self.dupes += 1;
            return Ok(false);
        }
        self.init_hash = Some(hash);
        if let Some(&earlier) = self.hash_part.get(&hash) {
            // Back to an earlier init (an ABR switch back): continue its
            // part, whose file already starts with this init.
            self.switch_part(dir, sb, earlier)?;
            self.parts_full = false;
            println!("sb{sb} init seen before, continuing part {earlier}");
            self.dupes += 1;
            return Ok(false);
        }
        // Media that came before the first init (the hook attached late)
        // stays alone in its part.
        if !self.hash_part.is_empty() || self.offset > 0 {
            if self.parts >= MAX_PARTS_PER_TRACK {
                // Each distinct init opens a file; a page can forge them.
                // Its media would land under another init: skipped until a
                // known init comes back.
                if !self.parts_full {
                    eprintln!("sb{sb}: more than {MAX_PARTS_PER_TRACK} parts; skipping media under new inits");
                }
                self.parts_full = true;
                return Ok(false);
            }
            self.parts_full = false;
            let part = self.parts;
            self.parts += 1;
            self.switch_part(dir, sb, part)?;
            println!("sb{sb} new init segment, starting part {part}");
        }
        self.hash_part.insert(hash, self.part);
        Ok(true)
    }

    /// Decides about a media segment (timed) or an untimed continuation.
    fn admit_media(&mut self, h: &SegmentHeader, payload: &[u8], seen_room: bool) -> Admit {
        let Some(t) = h.tfdt else {
            if self.skip_tail {
                self.dupes += 1;
                return Admit::Skip;
            }
            return Admit::Write { as_init: false };
        };
        self.skip_tail = false;
        // In sequence mode the browser places media one after the other and
        // rewrites timestampOffset itself; a later period may reuse tfdt
        // values. A re-append after a seek has the same bytes, a new period
        // other bytes: the key there is the payload.
        let key = if h.seq_mode {
            (fnv1a(payload), i64::MIN, payload.len() as u64)
        } else {
            (t, tso_millis(h.tso), h.dur.unwrap_or(0))
        };
        if self.seen.contains(&key) {
            self.dupes += 1;
            self.skip_tail = true;
            return Admit::Skip;
        }
        if !seen_room {
            self.skipped += 1;
            return Admit::Skip;
        }
        self.seen.insert(key);
        Admit::Write { as_init: false }
    }
}

impl Session {
    fn open(
        out: &Path,
        session_id: &str,
        url: &str,
        title: &str,
        budget: Arc<Budget>,
    ) -> io::Result<Option<Self>> {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let meta = serde_json::json!({ "url": url, "title": title, "session": session_id, "started": secs });
        let meta = serde_json::to_vec_pretty(&meta)?;
        // The directory and session.json count against the budget too; it
        // is charged before anything is created, so a spent process budget
        // creates no further directories.
        let open_cost = meta.len() as u64 + 4096;
        if open_cost > budget.per_session || !budget.charge(open_cost) {
            return Ok(None);
        }
        let short = sanitize(session_id, 8);
        let base = format!("{secs}_{}", if short.is_empty() { "anon" } else { &short });
        // A reconnect of the same session within the same second must not
        // append to the previous directory (its offsets would restart at 0).
        let mut n = 0u32;
        let dir = loop {
            let name = if n == 0 {
                base.clone()
            } else {
                format!("{base}-{n}")
            };
            let dir = out.join(name);
            match fs::create_dir(&dir) {
                Ok(()) => break dir,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists && n < 1000 => n += 1,
                Err(e) => return Err(e),
            }
        };
        // Written atomically: a reader never sees half a file.
        let tmp = dir.join("session.json.tmp");
        fs::write(&tmp, &meta)?;
        fs::rename(&tmp, dir.join("session.json"))?;
        let index = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("index.jsonl"))?;
        Ok(Some(Self {
            dir,
            index,
            tracks: HashMap::new(),
            track_limit_logged: false,
            bytes_total: open_cost,
            budget,
            stopped: false,
            seen_total: 0,
            seen_limit_logged: false,
        }))
    }

    fn open_part(dir: &Path, sb: u32, part: u32) -> io::Result<File> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(format!("sb{sb}.p{part}.bin")))
    }

    /// Stops the session for good when `cost` more bytes do not fit the
    /// session or the process budget.
    fn within_budget(&mut self, cost: u64) -> bool {
        if self.stopped {
            return false;
        }
        let session_ok = self.bytes_total.saturating_add(cost) <= self.budget.per_session;
        if session_ok && self.budget.charge(cost) {
            self.bytes_total += cost;
            return true;
        }
        self.stopped = true;
        eprintln!(
            "{}: {} budget reached (--max-session-gib {} / --max-total-gib {}); nothing more is written in this session",
            self.dir.display(),
            if session_ok { "process" } else { "session" },
            self.budget.per_session >> 30,
            self.budget.total_max >> 30
        );
        false
    }

    /// Writes one row: payload into the track's current part, a line into
    /// the index, both against the budget.
    fn write_row(
        &mut self,
        sb: u32,
        h: &SegmentHeader,
        payload: &[u8],
        as_init: bool,
    ) -> io::Result<()> {
        let track = self.tracks.get_mut(&sb).expect("present");
        let line = IndexLine {
            sb,
            ms: h.ms,
            part: track.part,
            seq: h.seq,
            offset: track.offset,
            len: payload.len(),
            is_init: as_init,
            tfdt: if as_init { None } else { h.tfdt },
            timescale: h.timescale,
            dur: if as_init { None } else { h.dur },
            tso: h.tso.filter(|v| v.is_finite()),
            seq_mode: h.seq_mode,
            tso_set: h.tso_set && !as_init,
            mime: &h.mime,
            kind: &h.kind,
        };
        // Serialized first and written in one call: an interrupted sink
        // leaves at most a missing line, not half of one.
        let mut line_buf = serde_json::to_vec(&line)?;
        line_buf.push(b'\n');
        let cost = (payload.len() + line_buf.len()) as u64;
        if !self.within_budget(cost) {
            return Ok(());
        }
        let track = self.tracks.get_mut(&sb).expect("present");
        track.file.write_all(payload)?;
        self.index.write_all(&line_buf)?;
        track.offset += payload.len() as u64;
        track.bytes += payload.len() as u64;
        track.segments += 1;
        Ok(())
    }

    /// Decides about and writes a track's pending init (all its chunks).
    fn flush_init(&mut self, sb: u32) -> io::Result<()> {
        let dir = self.dir.clone();
        let Some(track) = self.tracks.get_mut(&sb) else {
            return Ok(());
        };
        if track.pending.is_empty() {
            return Ok(());
        }
        let (chunks, whole) = track.take_pending();
        let parts_before = track.parts;
        let write = track.decide_init(fnv1a(&whole), &dir, sb)?;
        let new_file = track.parts != parts_before;
        // A new part file costs more than its bytes.
        if (new_file && !self.within_budget(PART_FILE_COST)) || !write {
            return Ok(());
        }
        let mut at = 0;
        for (h, len) in &chunks {
            self.write_row(sb, h, &whole[at..at + len], true)?;
            at += len;
        }
        Ok(())
    }

    /// The extension dropped an init of `sb` (its backlog was full) and
    /// nothing of `sb` after it: recorded in `events.jsonl` for `mux`.
    fn note_lost_init(&mut self, sb: u32) -> io::Result<()> {
        let mut line = serde_json::to_vec(&serde_json::json!({ "sb": sb, "event": "initlost" }))?;
        line.push(b'\n');
        if !self.within_budget(line.len() as u64) {
            return Ok(());
        }
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("events.jsonl"))?
            .write_all(&line)
    }

    /// Decides about every pending init (end of a connection or stream).
    fn finish(&mut self) {
        let ids: Vec<u32> = self.tracks.keys().copied().collect();
        for sb in ids {
            if let Err(e) = self.flush_init(sb) {
                eprintln!("{}: sb{sb}: init not written: {e}", self.dir.display());
            }
        }
    }

    /// Creates the track of `sb` on its first segment; false when it is over
    /// the track limit or the budget.
    fn ensure_track(&mut self, sb: u32) -> io::Result<bool> {
        if self.tracks.contains_key(&sb) {
            return Ok(true);
        }
        if self.tracks.len() >= MAX_TRACKS {
            if !self.track_limit_logged {
                eprintln!(
                    "{}: more than {MAX_TRACKS} SourceBuffers, ignoring sb{sb} and further new ones",
                    self.dir.display()
                );
                self.track_limit_logged = true;
            }
            return Ok(false);
        }
        if !self.within_budget(PART_FILE_COST) {
            return Ok(false);
        }
        let file = Self::open_part(&self.dir, sb, 0)?;
        self.tracks.insert(sb, TrackState::new(file));
        Ok(true)
    }

    fn write_segment(&mut self, h: &SegmentHeader, payload: &[u8]) -> io::Result<()> {
        if self.stopped || payload.is_empty() {
            return Ok(());
        }
        // An init with its first segment in one append: two rows, so the init
        // is decided (and deduped) on its own bytes.
        if h.is_init {
            if let Some(n) = init_prefix_len(&h.kind, payload) {
                let (init, media) = payload.split_at(n);
                let mut hi = h.clone();
                hi.tfdt = None;
                hi.dur = None;
                self.write_segment(&hi, init)?;
                // The init ends where the media starts: decide it now, so an
                // untimed media part (a moof without tfdt) is not taken for
                // its continuation.
                self.flush_init(h.sb)?;
                let mut hm = h.clone();
                hm.is_init = false;
                hm.init_cont = false;
                return self.write_segment(&hm, media);
            }
        }
        let dir = self.dir.clone();
        if !self.ensure_track(h.sb)? {
            return Ok(());
        }
        let track = self.tracks.get_mut(&h.sb).expect("present");
        // An untimed chunk continues the init when the extension says so, or
        // while the pending init is still incomplete. After a complete init it
        // is media (a segment cut inside its moof), not init bytes.
        let continuation = !h.is_init
            && h.tfdt.is_none()
            && (h.init_cont || (!track.pending.is_empty() && !track.pending_done));

        if payload_encrypted(h, payload, continuation, &track.init_kind) {
            if !track.refused {
                eprintln!(
                    "{}: sb{} carries encrypted media (DRM); not capturing this track",
                    dir.display(),
                    h.sb
                );
            }
            track.refused = true;
            track.take_pending();
            track.skipped += 1;
            return Ok(());
        }
        if track.refused && !h.is_init {
            track.skipped += 1;
            return Ok(());
        }

        if h.is_init {
            self.flush_init(h.sb)?;
            let track = self.tracks.get_mut(&h.sb).expect("present");
            // A clear init ends an encrypted stretch.
            track.pending_after_refusal = std::mem::take(&mut track.refused);
            track.init_kind.clone_from(&h.kind);
            track.push_pending(h, payload);
            if track.pending_cost() > MAX_PENDING_INIT {
                self.flush_init(h.sb)?;
            }
            return Ok(());
        }
        if continuation && !track.pending.is_empty() {
            if track.pending_cost() + payload.len() + PENDING_CHUNK_COST <= MAX_PENDING_INIT {
                track.push_pending(h, payload);
                return Ok(());
            }
            // Too large to hold: decide on what came so far, write the rest
            // as init bytes of that part.
            self.flush_init(h.sb)?;
            if self.tracks[&h.sb].parts_full {
                return Ok(());
            }
            return self.write_row(h.sb, h, payload, true);
        }
        self.flush_init(h.sb)?;
        let track = self.tracks.get_mut(&h.sb).expect("present");
        if track.parts_full {
            track.skipped += 1;
            return Ok(());
        }

        let seen_room = self.seen_total < MAX_SEEN_PER_SESSION;
        if !seen_room && !self.seen_limit_logged {
            eprintln!(
                "{}: {MAX_SEEN_PER_SESSION} media segments in this session; not taking more",
                dir.display()
            );
            self.seen_limit_logged = true;
        }
        let track = self.tracks.get_mut(&h.sb).expect("present");
        let Admit::Write { as_init } = track.admit_media(h, payload, seen_room) else {
            return Ok(());
        };
        if h.tfdt.is_some() {
            self.seen_total += 1;
        }
        self.write_row(h.sb, h, payload, as_init)
    }

    fn summary(&self) {
        let mut ids: Vec<_> = self.tracks.keys().copied().collect();
        ids.sort_unstable();
        for id in ids {
            let t = &self.tracks[&id];
            println!(
                "{}: sb{id} parts={} segments={} dupes={} skipped={} bytes={}",
                self.dir.display(),
                t.parts,
                t.segments,
                t.dupes,
                t.skipped,
                t.bytes
            );
        }
    }
}

// ---------------------------------------------------------------------------
// mux
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct IndexRow {
    sb: u32,
    #[serde(default)]
    ms: Option<u32>,
    part: u32,
    offset: u64,
    len: u64,
    is_init: bool,
    tfdt: Option<u64>,
    timescale: Option<u32>,
    #[serde(default)]
    dur: Option<u64>,
    #[serde(default)]
    tso: Option<f64>,
    #[serde(default)]
    seq_mode: bool,
    #[serde(default)]
    tso_set: bool,
    mime: String,
    #[serde(default)]
    kind: String,
}

/// Top-level ISO BMFF boxes in `buf[start..end]` as (type, body start, end).
fn boxes_in(buf: &[u8], start: usize, end: usize) -> Vec<([u8; 4], usize, usize)> {
    let mut out = Vec::new();
    let mut off = start;
    while off + 8 <= end {
        let size32 = u32::from_be_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
        let kind = [buf[off + 4], buf[off + 5], buf[off + 6], buf[off + 7]];
        let (size, header) = match size32 {
            0 => (end - off, 8),
            1 => {
                if off + 16 > end {
                    break;
                }
                let mut b = [0u8; 8];
                b.copy_from_slice(&buf[off + 8..off + 16]);
                match usize::try_from(u64::from_be_bytes(b)) {
                    Ok(s) => (s, 16),
                    Err(_) => break,
                }
            }
            s => (s as usize, 8),
        };
        if size < header || off.saturating_add(size) > end {
            break;
        }
        out.push((kind, off + header, off + size));
        off += size;
    }
    out
}

/// Adds `delta` to every `moof/traf/tfdt` of one media segment. Returns
/// false (and leaves the segment unchanged) when a result would be negative
/// or not fit a version-0 box.
#[cfg(test)]
fn shift_tfdt(seg: &mut [u8], delta: i64) -> bool {
    shift_tfdt_by(seg, &|_| delta)
}

/// Like `shift_tfdt`, with the delta per `traf` from its `tfhd` track ID:
/// each track counts in its own timescale.
fn shift_tfdt_by(seg: &mut [u8], delta_of: &dyn Fn(Option<u32>) -> i64) -> bool {
    let mut fields = Vec::new(); // (offset, is_64bit, delta)
    for (kind, s, e) in boxes_in(seg, 0, seg.len()) {
        if &kind != b"moof" {
            continue;
        }
        for (kind, s, e) in boxes_in(seg, s, e) {
            if &kind != b"traf" {
                continue;
            }
            let children = boxes_in(seg, s, e);
            let track = children
                .iter()
                .find(|(kind, s, e)| kind == b"tfhd" && *e >= s + 8)
                .map(|(_, s, _)| {
                    u32::from_be_bytes([seg[s + 4], seg[s + 5], seg[s + 6], seg[s + 7]])
                });
            let delta = delta_of(track);
            for (kind, s, e) in children {
                if &kind == b"tfdt" && e >= s + 8 {
                    let v1 = seg[s] == 1;
                    if !v1 || e >= s + 12 {
                        fields.push((s + 4, v1, delta));
                    }
                }
            }
        }
    }
    let mut writes = Vec::new();
    for &(off, v1, delta) in &fields {
        let old = if v1 {
            let mut b = [0u8; 8];
            b.copy_from_slice(&seg[off..off + 8]);
            i128::from(u64::from_be_bytes(b))
        } else {
            i128::from(u32::from_be_bytes([
                seg[off],
                seg[off + 1],
                seg[off + 2],
                seg[off + 3],
            ]))
        };
        let new = old + i128::from(delta);
        let fits = if v1 {
            u64::try_from(new).is_ok()
        } else {
            u32::try_from(new).is_ok()
        };
        if !fits {
            return false;
        }
        writes.push((off, v1, new));
    }
    for (off, v1, new) in writes {
        if v1 {
            let v = u64::try_from(new).expect("checked above");
            seg[off..off + 8].copy_from_slice(&v.to_be_bytes());
        } else {
            let v = u32::try_from(new).expect("checked above");
            seg[off..off + 4].copy_from_slice(&v.to_be_bytes());
        }
    }
    !fields.is_empty()
}

/// Track ID -> `mdhd` timescale of every `trak` in an init's `moov`.
fn trak_timescales(init: &[u8]) -> HashMap<u32, u32> {
    let be32 = |b: &[u8], o: usize| -> Option<u32> {
        b.get(o..o + 4)
            .map(|x| u32::from_be_bytes([x[0], x[1], x[2], x[3]]))
    };
    let mut out = HashMap::new();
    for (kind, s, e) in boxes_in(init, 0, init.len()) {
        if &kind != b"moov" {
            continue;
        }
        for (kind, s, e) in boxes_in(init, s, e) {
            if &kind != b"trak" {
                continue;
            }
            let mut id = None;
            let mut ts = None;
            for (kind, s, e) in boxes_in(init, s, e) {
                if &kind == b"tkhd" {
                    // fullbox; v0: ctime, mtime (4+4); v1: (8+8); then track_ID
                    let at = if init.get(s) == Some(&1) {
                        s + 20
                    } else {
                        s + 12
                    };
                    id = be32(&init[..e], at);
                } else if &kind == b"mdia" {
                    for (kind, s, e) in boxes_in(init, s, e) {
                        if &kind == b"mdhd" {
                            let at = if init.get(s) == Some(&1) {
                                s + 20
                            } else {
                                s + 12
                            };
                            ts = be32(&init[..e], at);
                        }
                    }
                }
            }
            if let (Some(id), Some(ts)) = (id, ts) {
                out.insert(id, ts);
            }
        }
    }
    out
}

/// Number of `trak` boxes in the `moov` of an init segment.
fn trak_count(init: &[u8]) -> usize {
    boxes_in(init, 0, init.len())
        .iter()
        .filter(|(kind, _, _)| kind == b"moov")
        .map(|(_, s, e)| {
            boxes_in(init, *s, *e)
                .iter()
                .filter(|(kind, _, _)| kind == b"trak")
                .count()
        })
        .sum()
}

/// `timestampOffset` (seconds) as ticks of `timescale`, rounded once from
/// the exact product (no millisecond step in between).
fn shift_ticks(tso: Option<f64>, timescale: Option<u32>) -> i64 {
    match (tso.filter(|v| v.is_finite()), timescale) {
        (Some(o), Some(ts)) if ts > 0 => {
            // Clamped well inside i64, so the cast cannot truncate.
            #[allow(clippy::cast_possible_truncation)]
            let ticks = (o * f64::from(ts)).round().clamp(-9e15, 9e15) as i64;
            ticks
        }
        _ => 0,
    }
}

/// Ticks of `from` as ticks of `to`, rounded (0 without a source timescale).
fn rescale(ticks: i64, from: Option<u32>, to: u32) -> i64 {
    let Some(src) = from.filter(|s| *s > 0) else {
        return 0;
    };
    let num = i128::from(ticks) * i128::from(to);
    let den = i128::from(src);
    let quot = (num + num.signum() * den / 2) / den;
    i64::try_from(quot.clamp(i128::from(i64::MIN), i128::from(i64::MAX))).expect("clamped")
}

/// Moves of sequence-mode media rows, per (part, offset), in ticks of each
/// row's timescale.
type SeqMoves = HashMap<(u32, u64), i64>;

/// Sequence mode: where the browser places each append (MSE coded frame
/// processing), per `SourceBuffer` over all its parts in arrival order (an
/// ABR switch and back keeps one timeline). After the player set
/// `timestampOffset` (`tso_set`: a seek, a resync) the append starts at that
/// offset, the group start. Otherwise it starts at its `tfdt` plus the offset
/// in effect, unless that does not continue within 100 ms of where the
/// previous append ended (a discontinuity, a new period restarting `tfdt`):
/// then the browser starts it right there. The browser puts the first
/// frame's presentation time at the group start: `leads` holds, per row, how
/// far that lies after its `tfdt` (composition offset of the first sample
/// minus the edit list's `media_time`). A row without `tfdt`, duration or
/// timescale gets no move and ends the group (the next one is not checked
/// against it). Returns the moves and the number of discontinuities.
fn seq_layout(rows: &[&IndexRow], leads: &SeqMoves) -> (SeqMoves, usize) {
    const NS: i128 = 1_000_000_000;
    let mut moves = HashMap::new();
    let mut group_end: Option<i128> = None;
    let mut jumps = 0;
    for r in rows.iter().filter(|r| !r.is_init && r.seq_mode) {
        let (Some(t), Some(d), Some(ts)) = (
            r.tfdt,
            r.dur.filter(|d| *d > 0),
            r.timescale.filter(|t| *t > 0),
        ) else {
            group_end = None;
            continue;
        };
        let ts = i128::from(ts);
        let t_ns = i128::from(t) * NS / ts;
        let tso_ns = i128::from(shift_ticks(r.tso, Some(1_000_000_000)));
        let start = if r.tso_set {
            let lead = leads.get(&(r.part, r.offset)).copied().unwrap_or(0);
            tso_ns - i128::from(lead) * NS / ts
        } else {
            let s = t_ns + tso_ns;
            match group_end {
                Some(e) if (s - e).abs() > NS / 10 => {
                    jumps += 1;
                    e
                }
                _ => s,
            }
        };
        group_end = Some(start + i128::from(d) * NS / ts);
        let num = (start - t_ns) * ts;
        let ticks = (num + num.signum() * NS / 2) / NS;
        let clamped = ticks.clamp(i128::from(i64::MIN), i128::from(i64::MAX));
        moves.insert((r.part, r.offset), i64::try_from(clamped).expect("clamped"));
    }
    (moves, jumps)
}

/// Big-endian integer of `n` bytes (at most 8) at `at`.
fn be_uint(b: &[u8], at: usize, n: usize) -> Option<u64> {
    b.get(at..at + n)
        .map(|x| x.iter().fold(0u64, |v, byte| (v << 8) | u64::from(*byte)))
}

/// Composition offset of the first sample of a media segment (first `trun`
/// of the first `traf`), in its track's ticks; 0 when it has none.
fn first_cto(seg: &[u8]) -> i64 {
    let Some((_, ms, me)) = boxes_in(seg, 0, seg.len())
        .into_iter()
        .find(|(k, _, _)| k == b"moof")
    else {
        return 0;
    };
    let Some((_, ts, te)) = boxes_in(seg, ms, me)
        .into_iter()
        .find(|(k, _, _)| k == b"traf")
    else {
        return 0;
    };
    let Some((_, s, e)) = boxes_in(seg, ts, te)
        .into_iter()
        .find(|(k, _, _)| k == b"trun")
    else {
        return 0;
    };
    let trun = &seg[s..e];
    let (Some(version), Some(flags)) = (be_uint(trun, 0, 1), be_uint(trun, 1, 3)) else {
        return 0;
    };
    if flags & 0x800 == 0 {
        return 0;
    }
    // sample_count, then optional data_offset and first_sample_flags, then
    // the first sample's duration, size and flags before its offset.
    let mut at = 8;
    for (bit, len) in [(0x1, 4), (0x4, 4), (0x100, 4), (0x200, 4), (0x400, 4)] {
        if flags & bit != 0 {
            at += len;
        }
    }
    let Some(raw) = be_uint(trun, at, 4) else {
        return 0;
    };
    let raw = u32::try_from(raw).expect("4 bytes");
    if version == 1 {
        i64::from(i32::from_be_bytes(raw.to_be_bytes()))
    } else {
        i64::from(raw)
    }
}

/// `media_time` of the first non-empty edit of the first track's edit list
/// (`moov/trak/edts/elst`), in its media ticks; 0 without one.
fn elst_media_time(init: &[u8]) -> i64 {
    let find = |s: usize, e: usize, kind: &[u8; 4]| {
        boxes_in(init, s, e)
            .into_iter()
            .find(|(k, _, _)| k == kind)
            .map(|(_, s, e)| (s, e))
    };
    let Some((s, e)) = find(0, init.len(), b"moov")
        .and_then(|(s, e)| find(s, e, b"trak"))
        .and_then(|(s, e)| find(s, e, b"edts"))
        .and_then(|(s, e)| find(s, e, b"elst"))
    else {
        return 0;
    };
    let elst = &init[s..e];
    let v1 = elst.first() == Some(&1);
    let count = be_uint(elst, 4, 4).unwrap_or(0);
    let entry = if v1 { 16 } else { 8 };
    for i in 0..count.min(16) {
        let at = 8 + usize::try_from(i).expect("small") * entry;
        let media_time = if v1 {
            be_uint(elst, at + 8, 8).map(|v| i64::from_be_bytes(v.to_be_bytes()))
        } else {
            be_uint(elst, at + 4, 4).map(|v| {
                i64::from(i32::from_be_bytes(
                    u32::try_from(v).expect("4 bytes").to_be_bytes(),
                ))
            })
        };
        match media_time {
            Some(-1) => {} // an empty edit
            Some(t) => return t,
            None => return 0,
        }
    }
    0
}

/// The presentation lead (`seq_layout`) of every sequence-mode row appended
/// right after a set `timestampOffset`: read from its segment and its part's
/// init. Unreadable rows get none.
fn seq_leads(dir: &Path, sb: u32, rows: &[&IndexRow]) -> SeqMoves {
    let mut leads = HashMap::new();
    let mut parts: Vec<u32> = rows
        .iter()
        .filter(|r| r.seq_mode && r.tso_set && !r.is_init)
        .map(|r| r.part)
        .collect();
    parts.sort_unstable();
    parts.dedup();
    let mut buf = Vec::new();
    for part in parts {
        let Ok(mut src) = File::open(dir.join(format!("sb{sb}.p{part}.bin"))) else {
            continue;
        };
        let mut init = Vec::new();
        for r in rows.iter().filter(|r| r.part == part && r.is_init) {
            if read_segment(&mut src, r, &mut buf).is_ok() {
                init.extend_from_slice(&buf);
            }
        }
        let media_time = elst_media_time(&init);
        for r in rows
            .iter()
            .filter(|r| r.part == part && r.seq_mode && r.tso_set && !r.is_init)
        {
            if read_segment(&mut src, r, &mut buf).is_ok() {
                leads.insert((part, r.offset), first_cto(&buf) - media_time);
            }
        }
    }
    leads
}

/// The sequence-mode layout of every `SourceBuffer` (`seq_layout`), and the
/// lift (nanoseconds) for all of them. A frame placed by its offset may need
/// a decode time before 0 (B-frames: at a set offset the browser puts the
/// presentation time there; a negative `timestampOffset`), which an MP4
/// cannot hold: then every `SourceBuffer` moves later by the same amount, so
/// their alignment stays.
fn seq_layouts(dir: &Path, rows: &[IndexRow], ids: &[u32]) -> (HashMap<u32, SeqMoves>, i64) {
    const NS: i128 = 1_000_000_000;
    let mut layouts = HashMap::new();
    let mut earliest: i128 = 0;
    for id in ids {
        let sb_rows: Vec<&IndexRow> = rows.iter().filter(|r| r.sb == *id).collect();
        let leads = seq_leads(dir, *id, &sb_rows);
        let (moves, jumps) = seq_layout(&sb_rows, &leads);
        if jumps > 0 {
            eprintln!(
                "mux: sb{id}: sequence mode: {jumps} discontinuity(ies) without a timestampOffset set; placed right after the previous append, as the browser does"
            );
        }
        for r in sb_rows.iter().filter(|r| !r.is_init) {
            let (Some(t), Some(ts)) = (r.tfdt, r.timescale.filter(|t| *t > 0)) else {
                continue;
            };
            let m = if r.seq_mode {
                moves.get(&(r.part, r.offset)).copied().unwrap_or(0)
            } else {
                shift_ticks(r.tso, Some(ts))
            };
            earliest = earliest.min((i128::from(t) + i128::from(m)) * NS / i128::from(ts));
        }
        layouts.insert(*id, moves);
    }
    let lift = i64::try_from((-earliest).clamp(0, i128::from(i64::MAX))).expect("clamped");
    if lift > 0 {
        eprintln!(
            "mux: the earliest frame would decode {:.3} s before 0; every SourceBuffer is moved later by that",
            Duration::from_nanos(lift.unsigned_abs()).as_secs_f64()
        );
    }
    (layouts, lift)
}

/// `ns` nanoseconds as ticks of `ts`, rounded up (0 without a timescale).
fn ns_ticks(ns: i64, ts: Option<u32>) -> i64 {
    let Some(ts) = ts.filter(|t| *t > 0) else {
        return 0;
    };
    let ticks = (i128::from(ns) * i128::from(ts) + 999_999_999) / 1_000_000_000;
    i64::try_from(ticks.clamp(i128::from(i64::MIN), i128::from(i64::MAX))).expect("clamped")
}

fn read_segment(src: &mut File, r: &IndexRow, buf: &mut Vec<u8>) -> io::Result<()> {
    let len = usize::try_from(r.len)
        .ok()
        .filter(|l| *l <= MAX_MESSAGE)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "segment too large"))?;
    buf.resize(len, 0);
    src.seek(io::SeekFrom::Start(r.offset))?;
    src.read_exact(buf)
}

/// Reports ranges skipped by a forward seek, from the sorted start times.
fn report_gaps(sb: u32, part: u32, starts: &[i128], ts: u32) {
    let steps: Vec<u64> = starts
        .windows(2)
        .filter_map(|w| u64::try_from(w[1] - w[0]).ok())
        .collect();
    let mut typical = steps.clone();
    typical.sort_unstable();
    let Some(&median) = typical.get(typical.len() / 2) else {
        return;
    };
    let threshold = median.saturating_add(median / 2);
    let big: Vec<u64> = steps.iter().copied().filter(|s| *s > threshold).collect();
    if !big.is_empty() {
        let missing = big
            .iter()
            .fold(0u64, |acc, s| acc.saturating_add(s.saturating_sub(median)));
        eprintln!(
            "mux: sb{sb}.p{part}: {} gap(s), about {} s not captured (seeked past them)",
            big.len(),
            missing / u64::from(ts)
        );
    }
}

/// Leaves out segments (sorted by start) that earlier ones fully cover, when
/// every segment has a duration. Returns the kept ones and the counts of
/// covered and partially overlapping segments.
fn drop_covered<'a>(
    order: Vec<&'a IndexRow>,
    pts: &dyn Fn(&IndexRow) -> i128,
) -> (Vec<&'a IndexRow>, usize, usize) {
    if !order.iter().all(|r| r.dur.is_some_and(|d| d > 0)) {
        return (order, 0, 0);
    }
    let (mut covered, mut partial) = (0, 0);
    let mut kept = Vec::with_capacity(order.len());
    let mut covered_end = i128::MIN;
    for r in order {
        let start = pts(r);
        let end = start + i128::from(r.dur.unwrap_or(0));
        if end <= covered_end {
            covered += 1;
            continue;
        }
        if start < covered_end {
            partial += 1;
        }
        covered_end = covered_end.max(end);
        kept.push(r);
    }
    (kept, covered, partial)
}

/// One part prepared for ffmpeg: its file, the demuxer to force, and the
/// presentation time (seconds) of its first media segment, if known.
#[derive(Debug)]
struct MuxInput {
    path: PathBuf,
    format: &'static str,
    start: Option<f64>,
}

/// Returns a file for `sbN.pM` with the init segment first and the media
/// segments in presentation order (`tfdt` plus `timestampOffset`), plus the
/// start time of its first media segment. Players fetch out of order after
/// a backward seek, and the sink writes in arrival order. If the raw part
/// already is in that order it is returned unchanged; otherwise
/// `sbN.pM.sorted.bin` is written.
///
/// - Segments moved by a `timestampOffset` get their `tfdt` rewritten so
///   the timeline stays monotonic; not for a `SourceBuffer` with several
///   tracks (each `traf` counts in its own timescale), which is warned.
/// - With durations on every segment, a segment fully covered by earlier
///   ones (an LL-HLS full segment after its parts, a re-fetch with another
///   cut) is left out; partial overlaps are kept and counted.
/// - Sequence mode: laid out one after the other in arrival order, as the
///   browser plays it (`seq_layout`); a jump (a seek, a new period) gets its
///   `tfdt` rewritten.
/// - Without a `tfdt` on every media segment (`WebM` blocks, continuation
///   chunks), or in sequence mode without durations, the arrival order is
///   kept.
fn ordered_part(
    dir: &Path,
    sb: u32,
    part: u32,
    rows: &[&IndexRow],
    seq_moves: Option<&SeqMoves>,
    lift_ns: i64,
) -> io::Result<(PathBuf, Option<f64>, usize)> {
    let raw = dir.join(format!("sb{sb}.p{part}.bin"));
    let init: Vec<&IndexRow> = rows.iter().copied().filter(|r| r.is_init).collect();
    let media: Vec<&IndexRow> = rows.iter().copied().filter(|r| !r.is_init).collect();
    // The media rows carry the timescale of their tfdt; the first row of a
    // split init may still carry the previous rendition's.
    let ts = media
        .iter()
        .chain(init.iter())
        .find_map(|r| r.timescale)
        .filter(|t| *t > 0);
    let mut src = File::open(&raw)?;
    let mut buf = Vec::new();

    // The init may be several rows (an init split across appends).
    let mut init_bytes = Vec::new();
    for r in &init {
        read_segment(&mut src, r, &mut buf)?;
        init_bytes.extend_from_slice(&buf);
    }
    let raw_tracks = trak_count(&init_bytes);
    let tracks = raw_tracks.max(1);
    // Per-track timescales let a multi-track segment be shifted traf by traf.
    let track_ts = trak_timescales(&init_bytes);
    // Sequence mode: placed where the browser placed it (`seq_layout`, over
    // all parts of the SourceBuffer); without that layout the arrival order.
    let seq_ok = media
        .iter()
        .filter(|r| r.seq_mode)
        .all(|r| seq_moves.is_some_and(|m| m.contains_key(&(part, r.offset))));
    // Every SourceBuffer moves by the same lift, so none decodes before 0.
    let lift = ns_ticks(lift_ns, ts);
    let offset = |r: &IndexRow| {
        lift + if r.seq_mode {
            seq_moves
                .and_then(|m| m.get(&(part, r.offset)))
                .copied()
                .unwrap_or(0)
        } else {
            shift_ticks(r.tso, ts)
        }
    };
    let wants_shift = media.iter().any(|r| offset(r) != 0);
    let can_shift = tracks == 1 || (raw_tracks > 0 && track_ts.len() == raw_tracks);
    if wants_shift && !can_shift {
        eprintln!(
            "mux: sb{sb}.p{part}: {tracks} tracks in one SourceBuffer without per-track timescales; timestampOffset is not written into the segments, the timeline may jump"
        );
    }
    // Order and coverage always follow the presentation time; the tfdt is
    // only rewritten where one timescale holds for the whole segment.
    let delta = |r: &IndexRow| if can_shift { offset(r) } else { 0 };
    let pts = |r: &IndexRow| i128::from(r.tfdt.unwrap_or(0)) + i128::from(offset(r));
    // Sequence mode needs durations to lay out the jumps; without them the
    // arrival order (duplicates already removed by the sink) is kept.
    let timed = !media.is_empty() && media.iter().all(|r| r.tfdt.is_some()) && seq_ok;
    let init_first = rows.iter().take(init.len()).all(|r| r.is_init);

    let mut order: Vec<&IndexRow> = media.clone();
    if timed {
        // Longer first at the same start, so the shorter one is the covered one.
        order.sort_by_key(|r| (pts(r), std::cmp::Reverse(r.dur.unwrap_or(0))));
        let (covered, partial);
        (order, covered, partial) = drop_covered(order, &pts);
        if let Some(ts) = ts {
            let starts: Vec<i128> = order.iter().map(|r| pts(r)).collect();
            report_gaps(sb, part, &starts, ts);
        }
        if covered > 0 || partial > 0 {
            eprintln!(
                "mux: sb{sb}.p{part}: {covered} segment(s) fully covered by others left out, {partial} partial overlap(s) kept (frames may repeat there)"
            );
        }
        if !media.iter().all(|r| r.dur.is_some_and(|d| d > 0)) {
            eprintln!(
                "mux: sb{sb}.p{part}: segment durations unknown; overlapping re-fetches cannot be detected"
            );
        }
    } else {
        warn_arrival_order(sb, part, &media);
    }

    // Start time of the first media segment, for aligning the inputs.
    let start = match (timed, ts, order.first()) {
        (true, Some(ts), Some(r)) => {
            // Presentation times are far below 2^52 ticks in practice; a
            // rounding error here only moves the alignment by a tick.
            #[allow(clippy::cast_precision_loss)]
            let secs = pts(r) as f64 / f64::from(ts);
            Some(secs)
        }
        _ => None,
    };

    let same_order = order.len() == media.len()
        && order
            .iter()
            .zip(media.iter())
            .all(|(a, b)| std::ptr::eq(*a, *b));
    let shifted = can_shift && wants_shift;
    if init_first && !shifted && same_order {
        return Ok((raw, start, raw_tracks));
    }

    // The tfdt shift of a row's traf: per track in a multi-track part, the
    // row's own otherwise (tid None: the row as a whole).
    let shift_for = |r: &IndexRow, tid: Option<u32>| -> i64 {
        if r.is_init {
            return 0;
        }
        match (r.seq_mode, tid.and_then(|t| track_ts.get(&t))) {
            (false, Some(&t)) if tracks > 1 => {
                shift_ticks(r.tso, Some(t)) + ns_ticks(lift_ns, Some(t))
            }
            (true, Some(&t)) if tracks > 1 => rescale(offset(r), ts, t),
            _ => delta(r),
        }
    };
    let rows_out: Vec<&IndexRow> = init.iter().chain(order.iter()).copied().collect();
    let sorted = write_sorted(dir, (sb, part), &mut src, &rows_out, &shift_for, shifted)?;
    Ok((sorted, start, raw_tracks))
}

/// Says why a part keeps its arrival order.
fn warn_arrival_order(sb: u32, part: u32, media: &[&IndexRow]) {
    if media.is_empty() {
        return;
    }
    let untimed = media.iter().filter(|r| r.tfdt.is_none()).count();
    if untimed > 0 {
        eprintln!(
            "mux: sb{sb}.p{part}: {untimed} media segment(s) without tfdt; keeping arrival order"
        );
    } else {
        eprintln!(
            "mux: sb{sb}.p{part}: sequence mode without segment durations; keeping arrival order, a seek or a new period may jump back in time"
        );
    }
}

/// Writes `sbN.pM.sorted.bin` from `rows` in that order, shifting each
/// media segment's tfdt by `shift_for`.
fn write_sorted(
    dir: &Path,
    (sb, part): (u32, u32),
    src: &mut File,
    rows: &[&IndexRow],
    shift_for: &dyn Fn(&IndexRow, Option<u32>) -> i64,
    shifted: bool,
) -> io::Result<PathBuf> {
    let sorted = dir.join(format!("sb{sb}.p{part}.sorted.bin"));
    let mut dst = io::BufWriter::new(File::create(&sorted)?);
    let mut buf = Vec::new();
    let mut unshifted = 0usize;
    for r in rows {
        read_segment(src, r, &mut buf)?;
        if shift_for(r, None) != 0 && !shift_tfdt_by(&mut buf, &|tid| shift_for(r, tid)) {
            unshifted += 1;
        }
        dst.write_all(&buf)?;
    }
    dst.flush()?;
    if unshifted > 0 {
        eprintln!(
            "mux: sb{sb}.p{part}: {unshifted} segment(s) could not be moved by their timestampOffset; the timeline may jump"
        );
    }
    eprintln!(
        "mux: sb{sb}.p{part}: reordered{}, wrote {}",
        if shifted {
            " and shifted by timestampOffset"
        } else {
            ""
        },
        sorted.display()
    );
    Ok(sorted)
}

fn run_mux(args: &[String], tools: &Tools) -> Result<(), SinkError> {
    let Some(dir) = args.first().filter(|a| !a.starts_with("--")) else {
        return Err(SinkError::Usage("capture directory required".into()));
    };
    let dir = PathBuf::from(dir);
    let out = flag(args, "--out").map_or_else(|| dir.join("out.mp4"), PathBuf::from);
    let timeout = match flag(args, "--timeout-secs").map(str::parse::<u64>) {
        None => Duration::from_secs(DEFAULT_MUX_TIMEOUT_SECS),
        Some(Ok(s)) if (1..=86_400).contains(&s) => Duration::from_secs(s),
        Some(_) => {
            return Err(SinkError::Usage(
                "--timeout-secs must be a whole number from 1 to 86400".into(),
            ))
        }
    };

    let index_path = dir.join("index.jsonl");
    let index = fs::read_to_string(&index_path)
        .map_err(|e| SinkError::io("cannot read", Some(index_path), e))?;

    // Fail before any work when ffmpeg is missing, and name the version the
    // mux ran with.
    println!("mux: {}", ffmpeg_version(tools)?);

    // A bad line (an interrupted sink, a forged record) costs that line only.
    let mut rows: Vec<IndexRow> = Vec::new();
    let mut bad_lines = 0usize;
    for line in index.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str(line) {
            Ok(r) => rows.push(r),
            Err(e) => {
                bad_lines += 1;
                if bad_lines <= 3 {
                    eprintln!("mux: bad index line skipped: {e}");
                }
            }
        }
    }
    if bad_lines > 3 {
        eprintln!("mux: {bad_lines} bad index lines skipped in total");
    }

    keep_main_media_source(&mut rows);
    report_lost_inits(&dir);
    let plan = mux_inputs(&dir, &rows, tools)?;
    for w in &plan.warnings {
        eprintln!("mux: {w}");
    }
    run_ffmpeg(tools, &plan.inputs, &out, timeout)
}

/// Names the `SourceBuffer`s whose capture ends early because the extension
/// dropped one of their inits (`events.jsonl`, written by the sink).
fn report_lost_inits(dir: &Path) {
    let Ok(events) = fs::read_to_string(dir.join("events.jsonl")) else {
        return;
    };
    let mut lost: Vec<u64> = events
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["event"] == "initlost")
        .filter_map(|v| v["sb"].as_u64())
        .collect();
    lost.sort_unstable();
    lost.dedup();
    for sb in lost {
        eprintln!(
            "mux: sb{sb}: the extension dropped an init (its queue was full); the media between it and the next init of sb{sb} is not captured"
        );
    }
}

/// The first line of `ffmpeg -version`, e.g. `ffmpeg version 7.1 ...`.
fn ffmpeg_version(tools: &Tools) -> Result<String, SinkError> {
    let out = proc::run(&Spec {
        tool: Tool::Ffmpeg,
        program: tools.program(Tool::Ffmpeg),
        args: vec!["-hide_banner".into(), "-version".into()],
        extra_env: Vec::new(),
        timeout: VERSION_TIMEOUT,
    })?;
    Ok(version_line(&String::from_utf8_lossy(&out.stdout)))
}

/// `ffmpeg version <v>` from the version output, without the copyright.
fn version_line(text: &str) -> String {
    let first = text.lines().next().unwrap_or("").trim();
    let cut = first.find(" Copyright").map_or(first, |i| &first[..i]);
    if cut.is_empty() {
        "ffmpeg version unknown (empty -version output)".to_owned()
    } else {
        printable(cut, 200)
    }
}

/// One `MediaSource` is one video. A preroll ad, the next video of a
/// single-page app or a hover preview adds more `MediaSources` to the same
/// session; keep the one with the most bytes and name the others.
fn keep_main_media_source(rows: &mut Vec<IndexRow>) {
    let mut per_ms: HashMap<Option<u32>, u64> = HashMap::new();
    for r in rows.iter() {
        *per_ms.entry(r.ms).or_default() += r.len;
    }
    let Some((&chosen, _)) = per_ms
        .iter()
        .max_by_key(|(ms, bytes)| (**bytes, std::cmp::Reverse(**ms)))
    else {
        return;
    };
    let name = |ms: Option<u32>| ms.map_or("?".to_owned(), |m| m.to_string());
    if per_ms.len() > 1 {
        let mut others: Vec<String> = per_ms
            .iter()
            .filter(|(ms, _)| **ms != chosen)
            .map(|(ms, b)| format!("{} ({b} bytes)", name(*ms)))
            .collect();
        others.sort();
        eprintln!(
            "mux: {} MediaSources in this capture; using #{}, leaving out {}",
            per_ms.len(),
            name(chosen),
            others.join(", ")
        );
    }
    rows.retain(|r| r.ms == chosen);
}

/// The ffmpeg demuxer for a part, after checking that the part starts like
/// one: the bytes come from the page, and ffmpeg's format probing would
/// otherwise follow whatever they claim to be (an MPD or a playlist pulls in
/// other files).
fn input_format(kind: &str, path: &Path) -> Option<&'static str> {
    let mut head = [0u8; 8];
    File::open(path).ok()?.read_exact(&mut head).ok()?;
    match kind {
        "mp4" if matches!(&head[4..8], b"ftyp" | b"moov" | b"styp") => Some("mp4"),
        "webm" if head[..4] == [0x1a, 0x45, 0xdf, 0xa3] => Some("matroska"),
        _ => None,
    }
}

/// What `mux_inputs` hands to ffmpeg, plus what the caller must tell the
/// user about ffprobe (ENG-13: a missing ffprobe is said, not swallowed).
#[derive(Debug)]
struct MuxPlan {
    inputs: Vec<MuxInput>,
    warnings: Vec<String>,
}

/// The ffmpeg inputs: per `SourceBuffer` the chosen part in timeline order.
fn mux_inputs(dir: &Path, rows: &[IndexRow], tools: &Tools) -> Result<MuxPlan, SinkError> {
    let mut parts: HashMap<(u32, u32), (u64, bool, String)> = HashMap::new();
    for row in rows {
        let e = parts
            .entry((row.sb, row.part))
            .or_insert((0, false, row.mime.clone()));
        e.0 += row.len;
        e.1 |= row.is_init;
    }
    if parts.is_empty() {
        return Err(SinkError::Capture("index is empty".into()));
    }
    let mut prober = Prober {
        tools,
        missing: false,
        warnings: Vec::new(),
    };

    // Adaptive players often start at a low quality and switch after the
    // first segment, which starts a new part. Pick, per SourceBuffer, the
    // decodable part (has an init segment) with the most bytes.
    let mut ids: Vec<u32> = parts.keys().map(|(sb, _)| *sb).collect();
    ids.sort_unstable();
    ids.dedup();
    let (layouts, lift) = seq_layouts(dir, rows, &ids);
    let mut inputs = Vec::new();
    for id in &ids {
        let mut candidates: Vec<_> = parts
            .iter()
            .filter(|((sb, _), _)| sb == id)
            .map(|((_, part), v)| (*part, v))
            .collect();
        candidates.sort_unstable_by_key(|(part, _)| *part);
        let total = candidates.len();
        let best = candidates
            .iter()
            .filter(|(_, (_, has_init, _))| *has_init)
            .max_by_key(|(_, (bytes, _, _))| *bytes);
        let Some((part, (bytes, _, mime))) = best else {
            eprintln!("mux: sb{id} has no part with an init segment; skipped. Capture with the hook active from page load.");
            continue;
        };
        if total > 1 {
            eprintln!(
                "mux: sb{id} has {total} parts (codec/quality changes); using part {part} ({bytes} bytes, {})",
                printable(mime, 120)
            );
        }
        let sb_rows: Vec<&IndexRow> = rows.iter().filter(|r| r.sb == *id).collect();
        let no_moves = SeqMoves::new();
        let seq_moves = layouts.get(id).unwrap_or(&no_moves);
        let part_rows: Vec<&IndexRow> = sb_rows
            .iter()
            .copied()
            .filter(|r| r.part == *part)
            .collect();
        let kind = part_rows
            .iter()
            .find(|r| r.is_init)
            .map_or("", |r| r.kind.as_str());
        let (path, start, traks) = ordered_part(dir, *id, *part, &part_rows, Some(seq_moves), lift)
            .map_err(|e| {
                SinkError::io(format!("sb{id}.p{part}: cannot reorder segments"), None, e)
            })?;
        // An MP4 init without a complete moov (cut short) cannot be decoded,
        // and ffmpeg would fail the whole mux with it.
        if kind == "mp4" && traks == 0 {
            eprintln!("mux: sb{id}.p{part}: its init has no complete moov; skipped");
            continue;
        }
        let Some(format) = input_format(kind, &path) else {
            eprintln!(
                "mux: sb{id}.p{part} does not start like {} media; skipped",
                if kind.is_empty() {
                    "known".to_owned()
                } else {
                    printable(kind, 16)
                }
            );
            continue;
        };
        // The first presentation time as ffmpeg sees it (B-frame composition
        // offsets and edit lists included): that is what ffmpeg subtracts.
        // A part ffprobe cannot read is left out instead of failing the mux.
        // Without ffprobe (or when it fails to run) the part is kept with the
        // capture's own start, and that is said (ENG-13).
        let verdict = prober.probe(format, &path, &format!("sb{id}.p{part}"));
        let start = match verdict {
            Some(Probe::Undecodable) => {
                eprintln!("mux: sb{id}.p{part} cannot be decoded; skipped");
                continue;
            }
            Some(Probe::Start(s)) => Some(s),
            Some(Probe::Unknown) | None => start,
        };
        inputs.push(MuxInput {
            path,
            format,
            start,
        });
    }
    if inputs.is_empty() {
        return Err(SinkError::Capture("nothing decodable to mux".into()));
    }
    Ok(MuxPlan {
        inputs,
        warnings: prober.warnings,
    })
}

/// Runs ffprobe per part and collects what went wrong with ffprobe itself.
struct Prober<'a> {
    tools: &'a Tools,
    /// Set once ffprobe turned out to be missing or not startable: not asked
    /// again per part.
    missing: bool,
    warnings: Vec<String>,
}

impl Prober<'_> {
    /// `None` when ffprobe could not tell (a warning is recorded).
    fn probe(&mut self, format: &str, path: &Path, name: &str) -> Option<Probe> {
        if self.missing {
            return None;
        }
        match probe_start(self.tools, format, path) {
            Ok(p) => Some(p),
            // Missing or not startable: the same for every part, so said
            // once and not tried again (ENG-13).
            Err(e @ (SinkError::ToolMissing { .. } | SinkError::ToolSpawn { .. })) => {
                self.missing = true;
                self.warnings.push(format!(
                    "{e}. Without it the parts are not checked for decodability and are aligned by the capture's own timestamps (B-frame offsets and edit lists not counted)."
                ));
                None
            }
            Err(e) => {
                self.warnings.push(format!(
                    "{name}: {e}; aligned by the capture's own timestamps"
                ));
                None
            }
        }
    }
}

/// What ffprobe says about a part it could run on.
#[derive(Debug, PartialEq)]
enum Probe {
    /// First presentation time in seconds.
    Start(f64),
    /// Readable, no start time reported.
    Unknown,
    /// ffprobe failed on it.
    Undecodable,
}

/// `Err` only when ffprobe itself could not do its job (missing, not
/// startable, timed out, too much output); a part it rejects is
/// `Probe::Undecodable`.
fn probe_start(tools: &Tools, format: &str, path: &Path) -> Result<Probe, SinkError> {
    let mut args: Vec<std::ffi::OsString> = [
        "-v",
        "error",
        "-f",
        format,
        "-protocol_whitelist",
        "file",
        "-show_entries",
        "stream=codec_type:format=start_time",
        "-of",
        "default=noprint_wrappers=1",
    ]
    .iter()
    .map(Into::into)
    .collect();
    args.push(path.into());
    let out = proc::run(&Spec {
        tool: Tool::Ffprobe,
        program: tools.program(Tool::Ffprobe),
        args,
        extra_env: Vec::new(),
        timeout: PROBE_TIMEOUT,
    });
    match out {
        Err(SinkError::ToolFailed { .. }) => Ok(Probe::Undecodable),
        Err(e) => Err(e),
        Ok(o) => {
            let text = String::from_utf8_lossy(&o.stdout);
            // No audio or video stream (an init cut short reads "fine" but
            // has nothing to decode): ffmpeg would fail the whole mux.
            let has_stream = text
                .lines()
                .any(|l| l == "codec_type=video" || l == "codec_type=audio");
            if !has_stream {
                return Ok(Probe::Undecodable);
            }
            Ok(text
                .lines()
                .find_map(|l| l.strip_prefix("start_time="))
                .and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|v| v.is_finite())
                .map_or(Probe::Unknown, Probe::Start))
        }
    }
}

/// Input options of every input: the forced demuxer, files only, and an
/// offset that keeps the inputs aligned. ffmpeg moves every input to start
/// at 0, so a part that starts later (the video after a quality switch)
/// would otherwise slide against the others.
fn input_args(inputs: &[MuxInput]) -> Vec<Vec<String>> {
    let earliest = inputs
        .iter()
        .filter_map(|i| i.start)
        .fold(f64::INFINITY, f64::min);
    inputs
        .iter()
        .map(|i| {
            let mut a = vec![
                "-f".to_owned(),
                i.format.to_owned(),
                "-protocol_whitelist".to_owned(),
                "file".to_owned(),
            ];
            if let Some(s) = i.start.filter(|_| earliest.is_finite()) {
                let offset = s - earliest;
                if offset > 0.0 {
                    a.push("-itsoffset".to_owned());
                    a.push(format!("{offset:.6}"));
                }
            }
            a.push("-i".to_owned());
            a.push(i.path.to_string_lossy().into_owned());
            a
        })
        .collect()
}

/// The whole ffmpeg argv of the mux. Errors only (no progress, no
/// per-packet warnings): stderr is capped (`proc::PIPE_LIMIT`).
fn ffmpeg_args(inputs: &[MuxInput], out: &Path) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> =
        ["-hide_banner", "-nostats", "-loglevel", "error", "-y"]
            .iter()
            .map(Into::into)
            .collect();
    args.extend(input_args(inputs).into_iter().flatten().map(Into::into));
    // Every stream of every input, not ffmpeg's "one best video, one best
    // audio" pick across inputs.
    for n in 0..inputs.len() {
        args.push("-map".into());
        args.push(n.to_string().into());
    }
    args.extend(["-c", "copy", "-movflags", "+faststart"].map(Into::into));
    args.push(out.into());
    args
}

fn run_ffmpeg(
    tools: &Tools,
    inputs: &[MuxInput],
    out: &Path,
    timeout: Duration,
) -> Result<(), SinkError> {
    if inputs.iter().any(|i| i.start.is_none()) && inputs.len() > 1 {
        eprintln!("mux: some inputs have no start time; their alignment is ffmpeg's guess");
    }
    println!("mux: {} input(s) -> {}", inputs.len(), out.display());
    let done = write_through_part(out, |part| {
        proc::run(&Spec {
            tool: Tool::Ffmpeg,
            program: tools.program(Tool::Ffmpeg),
            args: ffmpeg_args(inputs, part),
            extra_env: Vec::new(),
            timeout,
        })
    })?;
    // Errors ffmpeg reported but survived are passed on, not dropped (RS-06).
    let notes = proc::quote_tail(&done.stderr, proc::STDERR_QUOTE);
    if !notes.is_empty() {
        eprintln!("mux: ffmpeg reported:\n{notes}");
    }
    println!("mux: ok");
    Ok(())
}

/// `out` with `.part` before its extension (`out.part.mp4`), next to it:
/// ffmpeg still picks the container from the extension.
fn part_path(out: &Path) -> PathBuf {
    let mut name = out.file_stem().unwrap_or_default().to_os_string();
    name.push(".part");
    if let Some(ext) = out.extension() {
        name.push(".");
        name.push(ext);
    }
    out.with_file_name(name)
}

/// Runs `write` on a sibling part file and renames it to `out` only on
/// success. A killed or failed ffmpeg leaves a truncated file (with
/// `+faststart` the moov comes last, so it does not play); that file is
/// removed, and an older `out` stays untouched.
fn write_through_part<T>(
    out: &Path,
    write: impl FnOnce(&Path) -> Result<T, SinkError>,
) -> Result<T, SinkError> {
    let part = part_path(out);
    let result = write(&part).and_then(|v| {
        fs::rename(&part, out)
            .map(|()| v)
            .map_err(|e| SinkError::io("cannot rename", Some(part.clone()), e))
    });
    if result.is_err() {
        if let Err(e) = fs::remove_file(&part) {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!("mux: cannot remove {}: {e}", part.display());
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    /// A fresh directory under the system temp directory, removed again when
    /// the guard drops: the name carries the pid, so a leftover would pile up
    /// with every run (CI on the author's machine included).
    struct TestDir(PathBuf);

    impl std::ops::Deref for TestDir {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl AsRef<Path> for TestDir {
        fn as_ref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temp_dir(name: &str) -> TestDir {
        let d = std::env::temp_dir().join(format!("streamtap-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).expect("temp dir");
        TestDir(d)
    }

    /// A session together with the directory it lives in; the session drops
    /// first (field order), so its files are closed when the directory goes.
    struct TestSession {
        s: Session,
        _dir: TestDir,
    }

    impl std::ops::Deref for TestSession {
        type Target = Session;
        fn deref(&self) -> &Session {
            &self.s
        }
    }

    impl std::ops::DerefMut for TestSession {
        fn deref_mut(&mut self) -> &mut Session {
            &mut self.s
        }
    }

    #[test]
    fn test_dirs_are_removed_when_dropped() {
        let dir = temp_dir("dropped");
        fs::write(dir.join("f"), b"x").expect("write");
        let path = dir.to_path_buf();
        drop(dir);
        assert!(!path.exists(), "{} left behind", path.display());
        let s = session("dropped-session");
        let root = s.dir.parent().expect("parent").to_path_buf();
        drop(s);
        assert!(!root.exists(), "{} left behind", root.display());
    }

    fn mp4_box(kind: [u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = u32::try_from(8 + body.len())
            .expect("small")
            .to_be_bytes()
            .to_vec();
        v.extend_from_slice(&kind);
        v.extend_from_slice(body);
        v
    }

    /// A minimal media segment: moof > traf > tfdt (version 0) with `tfdt`.
    fn moof(tfdt: u32) -> Vec<u8> {
        let mut tfdt_body = vec![0, 0, 0, 0];
        tfdt_body.extend_from_slice(&tfdt.to_be_bytes());
        mp4_box(*b"moof", &mp4_box(*b"traf", &mp4_box(*b"tfdt", &tfdt_body)))
    }

    fn tfdt_of(seg: &[u8]) -> u32 {
        u32::from_be_bytes([seg[28], seg[29], seg[30], seg[31]])
    }

    /// An init segment with `traks` empty trak boxes.
    fn init_with_traks(traks: usize) -> Vec<u8> {
        let mut moov = Vec::new();
        for _ in 0..traks {
            moov.extend(mp4_box(*b"trak", b""));
        }
        let mut v = mp4_box(*b"ftyp", b"iso6");
        v.extend(mp4_box(*b"moov", &moov));
        v
    }

    fn row(offset: u64, len: u64, is_init: bool, tfdt: Option<u64>, tso: Option<f64>) -> IndexRow {
        IndexRow {
            sb: 1,
            ms: Some(1),
            part: 0,
            offset,
            len,
            is_init,
            tfdt,
            timescale: Some(1000),
            dur: None,
            tso,
            seq_mode: false,
            tso_set: false,
            mime: "video/mp4".into(),
            kind: "mp4".into(),
        }
    }

    /// (bytes, `is_init`, tfdt, tso) of one test segment.
    type TestSeg<'a> = (&'a [u8], bool, Option<u64>, Option<f64>);

    /// Writes `segs` as sb1.p0.bin and returns the rows (with the given tfdt / tso).
    fn write_part(dir: &Path, segs: &[TestSeg<'_>]) -> Vec<IndexRow> {
        let mut raw = Vec::new();
        let mut rows = Vec::new();
        for (bytes, is_init, tfdt, tso) in segs {
            rows.push(row(
                raw.len() as u64,
                bytes.len() as u64,
                *is_init,
                *tfdt,
                *tso,
            ));
            raw.extend_from_slice(bytes);
        }
        fs::write(dir.join("sb1.p0.bin"), raw).expect("write part");
        rows
    }

    fn order(dir: &Path, rows: &[IndexRow]) -> (Vec<u8>, PathBuf, Option<f64>) {
        let refs: Vec<&IndexRow> = rows.iter().collect();
        // As mux_inputs does: the sequence-mode layout over the SourceBuffer.
        let leads = seq_leads(dir, 1, &refs);
        let (moves, _) = seq_layout(&refs, &leads);
        let (path, start, _) = ordered_part(dir, 1, 0, &refs, Some(&moves), 0).expect("ordered");
        (fs::read(&path).expect("read"), path, start)
    }

    #[test]
    fn tso_millis_handles_odd_values() {
        assert_eq!(tso_millis(None), 0);
        assert_eq!(tso_millis(Some(1.5)), 1500);
        assert_eq!(tso_millis(Some(-2.0)), -2000);
        assert_eq!(tso_millis(Some(f64::NAN)), 0);
        assert_eq!(tso_millis(Some(f64::INFINITY)), 0);
        assert_eq!(tso_millis(Some(1e300)), 1_000_000_000_000);
    }

    #[test]
    fn shift_ticks_is_exact_not_millisecond_rounded() {
        // tso = -tfdt/ts for tfdt 1001 at 30000. Rounding tso to whole
        // milliseconds first (-33 ms) would give -990 ticks instead of -1001.
        let tso = -1001.0 / 30000.0;
        assert_eq!(shift_ticks(Some(tso), Some(30000)), -1001);
        assert_eq!(shift_ticks(None, Some(1000)), 0);
        assert_eq!(shift_ticks(Some(1.0), None), 0);
    }

    #[test]
    fn encrypted_payloads_are_recognised() {
        assert!(looks_encrypted(b"\0\0\0\x10moov....encv...."));
        assert!(!looks_encrypted(b"\0\0\0\x10moov....avc1...."));
        let mut h = seg(1, false, Some(0), None);
        let senc = mp4_box(*b"moof", &mp4_box(*b"traf", &mp4_box(*b"senc", b"")));
        assert!(payload_encrypted(&h, &senc, false, "mp4"), "senc in a moof");
        assert!(!payload_encrypted(&h, &moof(0), false, "mp4"), "clear moof");
        let mut moov = mp4_box(*b"ftyp", b"iso6");
        moov.extend(mp4_box(*b"moov", b"..encv.."));
        assert!(
            payload_encrypted(&h, &moov, false, "mp4"),
            "moov in a chunk not flagged as init"
        );
        h.kind = "webm".into();
        h.is_init = true;
        assert!(payload_encrypted(
            &h,
            &[0x16, 0x54, 0x6d, 0x80, 0x50, 0x35, 0x47, 0xe2],
            false,
            "webm"
        ));
        assert!(!payload_encrypted(
            &h,
            &[0x16, 0x54, 0xae, 0x6b],
            false,
            "mp4"
        ));
    }

    #[test]
    fn shift_tfdt_moves_and_refuses_underflow() {
        let mut seg = moof(1000);
        assert!(shift_tfdt(&mut seg, 500));
        assert_eq!(tfdt_of(&seg), 1500);
        let before = seg.clone();
        assert!(!shift_tfdt(&mut seg, -2000), "negative result is refused");
        assert_eq!(seg, before, "a refused shift leaves the segment unchanged");
        let mut no_moof = b"\0\0\0\x08mdat".to_vec();
        assert!(!shift_tfdt(&mut no_moof, 1), "nothing to shift");
    }

    #[test]
    fn box_walker_survives_garbage_sizes() {
        for buf in [
            b"\0\0\0\x03moofxxxx".to_vec(),
            b"\0\0\x10\0moofxxxx".to_vec(),
            b"\0\0\0\x01moof\0\0".to_vec(),
        ] {
            assert_eq!(boxes_in(&buf, 0, buf.len()), Vec::new());
        }
    }

    #[test]
    fn trak_count_reads_the_init() {
        assert_eq!(trak_count(&init_with_traks(1)), 1);
        assert_eq!(trak_count(&init_with_traks(2)), 2);
        assert_eq!(trak_count(b"INIT"), 0);
    }

    #[test]
    fn ordered_part_sorts_by_tfdt_with_init_first() {
        let dir = temp_dir("sort");
        let (a, b) = (moof(2000), moof(1000));
        let rows = write_part(
            &dir,
            &[
                (b"INIT", true, None, None),
                (&a, false, Some(2000), None),
                (&b, false, Some(1000), None),
            ],
        );
        let (got, path, start) = order(&dir, &rows);
        assert!(path.ends_with("sb1.p0.sorted.bin"));
        assert_eq!(&got[..4], b"INIT");
        assert_eq!(tfdt_of(&got[4..]), 1000);
        assert_eq!(tfdt_of(&got[36..]), 2000);
        assert_eq!(start, Some(1.0), "first media at 1000 ticks of 1000/s");
    }

    #[test]
    fn ordered_part_keeps_arrival_order_without_tfdt_or_in_sequence_mode() {
        let dir = temp_dir("untimed");
        let rows = write_part(
            &dir,
            &[
                (b"INIT", true, None, None),
                (b"AAAA", false, Some(1000), None),
                (b"BBBB", false, None, None),
                (b"CCCC", false, Some(500), None),
            ],
        );
        let (got, path, start) = order(&dir, &rows);
        assert!(path.ends_with("sb1.p0.bin"), "already in arrival order");
        assert_eq!(got, b"INITAAAABBBBCCCC");
        assert_eq!(start, None);

        // Sequence mode: sorted by tfdt, the (stale) timestampOffset ignored.
        let mut rows = write_part(
            &dir,
            &[
                (b"INIT", true, None, None),
                (b"AAAA", false, Some(1000), None),
                (b"CCCC", false, Some(500), Some(99.0)),
            ],
        );
        for r in &mut rows {
            r.seq_mode = true;
        }
        assert_eq!(
            order(&dir, &rows).0,
            b"INITAAAACCCC",
            "sequence mode: arrival order (the sink dropped duplicates)"
        );
    }

    #[test]
    fn ordered_part_writes_every_init_row_first() {
        let dir = temp_dir("initrows");
        let rows = write_part(
            &dir,
            &[
                (b"IN", true, None, None),
                (b"IT", true, None, None),
                (b"BBBB", false, Some(2000), None),
                (b"AAAA", false, Some(1000), None),
            ],
        );
        assert_eq!(
            order(&dir, &rows).0,
            b"INITAAAABBBB",
            "a split init stays whole"
        );
    }

    #[test]
    fn ordered_part_moves_init_to_the_front() {
        let dir = temp_dir("initlate");
        let rows = write_part(
            &dir,
            &[(b"MMMM", false, None, None), (b"INIT", true, None, None)],
        );
        assert_eq!(order(&dir, &rows).0, b"INITMMMM");
    }

    #[test]
    fn ordered_part_applies_timestamp_offset() {
        let dir = temp_dir("tso");
        let (p1, p2) = (moof(5000), moof(0));
        let rows = write_part(
            &dir,
            &[
                (b"INIT", true, None, None),
                (&p1, false, Some(5000), None),
                (&p2, false, Some(0), Some(10.0)),
            ],
        );
        let got = order(&dir, &rows).0;
        assert_eq!(tfdt_of(&got[4..]), 5000);
        assert_eq!(
            tfdt_of(&got[36..]),
            10_000,
            "tfdt shifted by 10 s at timescale 1000"
        );
    }

    #[test]
    fn ordered_part_does_not_shift_multi_track_buffers() {
        let dir = temp_dir("multitrak");
        let init = init_with_traks(2);
        let p = moof(0);
        let rows = write_part(
            &dir,
            &[(&init, true, None, None), (&p, false, Some(0), Some(10.0))],
        );
        let got = order(&dir, &rows).0;
        assert_eq!(
            tfdt_of(&got[init.len()..]),
            0,
            "two tracks: no single timescale to shift by"
        );

        // A second period (tfdt restarts, tso 10 s) is neither dropped as
        // covered nor sorted into the first one.
        let (a, b) = (moof(0), moof(0));
        let mut rows = write_part(
            &dir,
            &[
                (&init, true, None, None),
                (&b, false, Some(0), Some(10.0)),
                (&a, false, Some(0), None),
            ],
        );
        rows[1].dur = Some(1000);
        rows[2].dur = Some(1000);
        let got = order(&dir, &rows).0;
        assert_eq!(got.len(), init.len() + 64, "both periods kept");
    }

    #[test]
    fn ordered_part_leaves_out_covered_segments() {
        let dir = temp_dir("overlap");
        let (part_a, part_b, full) = (moof(0), moof(1000), moof(0));
        let mut rows = write_part(
            &dir,
            &[
                (b"INIT", true, None, None),
                (&part_a, false, Some(0), None),
                (&part_b, false, Some(1000), None),
                (&full, false, Some(0), None),
            ],
        );
        rows[1].dur = Some(1000);
        rows[2].dur = Some(1000);
        rows[3].dur = Some(2000);
        let got = order(&dir, &rows).0;
        // The full segment sorts first (longer at the same start) and covers both parts.
        assert_eq!(got.len(), 4 + 32, "init + the full segment only");
    }

    #[test]
    fn gap_report_survives_huge_steps() {
        report_gaps(1, 0, &[0, 10, i128::from(u64::MAX)], 1000);
        report_gaps(
            1,
            0,
            &[0, i128::from(u64::MAX), i128::from(u64::MAX) * 2],
            1,
        );
    }

    #[test]
    fn mux_inputs_refuses_parts_that_are_not_media() {
        let dir = temp_dir("magic");
        let mpd = b"<?xml version=\"1.0\"?><MPD/>";
        fs::write(dir.join("sb1.p0.bin"), mpd).expect("write");
        let mut r = row(0, mpd.len() as u64, true, None, None);
        r.kind = "mp4".into();
        assert!(
            matches!(
                mux_inputs(&dir, &[r], &Tools::default()),
                Err(SinkError::Capture(_))
            ),
            "an MPD is not an mp4 part"
        );
        assert_eq!(input_format("mp4", &dir.join("sb1.p0.bin")), None);

        let init = init_with_traks(1);
        fs::write(dir.join("sb1.p0.bin"), &init).expect("write");
        assert_eq!(input_format("mp4", &dir.join("sb1.p0.bin")), Some("mp4"));
        assert_eq!(
            input_format("webm", &dir.join("sb1.p0.bin")),
            None,
            "kind and magic must agree"
        );
    }

    #[test]
    fn inputs_are_aligned_by_their_start() {
        let inputs = [
            MuxInput {
                path: PathBuf::from("v"),
                format: "mp4",
                start: Some(2.08),
            },
            MuxInput {
                path: PathBuf::from("a"),
                format: "mp4",
                start: Some(0.0),
            },
        ];
        let args = input_args(&inputs);
        assert!(args[0]
            .windows(2)
            .any(|w| w[0] == "-itsoffset" && w[1] == "2.080000"));
        assert!(
            !args[1].iter().any(|a| a == "-itsoffset"),
            "the earliest input is not moved"
        );
        assert!(args[0].windows(2).any(|w| w[0] == "-f" && w[1] == "mp4"));
    }

    #[test]
    fn split_message_rejects_malformed_frames() {
        assert!(split_message(b"\x01\0").is_err());
        assert!(
            split_message(b"\xff\xff\0\0{}").is_err(),
            "header longer than the message"
        );
        let mut bad = 5u32.to_le_bytes().to_vec();
        bad.extend_from_slice(b"{bad}");
        assert!(split_message(&bad).is_err());
        let frame = |json: &[u8]| {
            let mut f = u32::try_from(json.len())
                .expect("small")
                .to_le_bytes()
                .to_vec();
            f.extend_from_slice(json);
            f
        };
        assert!(
            split_message(&frame(br#"{"type":"segment","sb":"x","seq":1}"#)).is_err(),
            "a string sb id is rejected"
        );
        let big = frame(br#"{"type":"segment","sb":1,"seq":1,"tfdt":"18446744073709551615"}"#);
        match split_message(&big).expect("tfdt as a decimal string").0 {
            Header::Segment(h) => assert_eq!(h.tfdt, Some(u64::MAX)),
            _ => panic!("not a segment"),
        }
    }

    fn seg(sb: u32, is_init: bool, tfdt: Option<u64>, tso: Option<f64>) -> SegmentHeader {
        SegmentHeader {
            sb,
            seq: 1,
            ms: Some(1),
            mime: "video/mp4".into(),
            is_init,
            init_cont: false,
            kind: "mp4".into(),
            tfdt,
            timescale: Some(1000),
            dur: None,
            tso,
            seq_mode: false,
            tso_set: false,
        }
    }

    fn budget(per_session: u64, total_max: u64) -> Arc<Budget> {
        Arc::new(Budget {
            per_session,
            total_max,
            total_used: AtomicU64::new(0),
        })
    }

    fn session(name: &str) -> TestSession {
        let dir = temp_dir(name);
        let s = Session::open(&dir, "test", "u", "t", budget(16 * GIB, 64 * GIB))
            .expect("session")
            .expect("within budget");
        TestSession { s, _dir: dir }
    }

    fn index_lines(dir: &Path) -> usize {
        fs::read_to_string(dir.join("index.jsonl"))
            .expect("index")
            .lines()
            .count()
    }

    fn write(s: &mut Session, h: &SegmentHeader, bytes: &[u8]) {
        s.write_segment(h, bytes).expect("write");
    }

    #[test]
    fn dedupe_includes_timestamp_offset_and_duration() {
        let mut s = session("dedupe");
        write(&mut s, &seg(1, true, None, None), b"INIT");
        write(&mut s, &seg(1, false, Some(0), None), b"P1");
        write(&mut s, &seg(1, false, Some(0), None), b"P1");
        write(&mut s, &seg(1, false, Some(0), Some(10.0)), b"P2");
        let mut longer = seg(1, false, Some(0), None);
        longer.dur = Some(4000);
        write(&mut s, &longer, b"FULL");
        assert_eq!(
            index_lines(&s.dir),
            4,
            "one dupe dropped; other offset and duration kept"
        );
    }

    #[test]
    fn continuation_of_a_duplicate_is_dropped() {
        let mut s = session("tail");
        write(&mut s, &seg(1, true, None, None), b"INIT");
        write(&mut s, &seg(1, false, Some(0), None), b"HEAD");
        write(&mut s, &seg(1, false, None, None), b"TAIL");
        write(&mut s, &seg(1, false, Some(0), None), b"HEAD");
        write(&mut s, &seg(1, false, None, None), b"TAIL");
        write(&mut s, &seg(1, false, Some(1000), None), b"NEXT");
        assert_eq!(index_lines(&s.dir), 4, "init, head, tail, next");
        assert_eq!(
            fs::read(s.dir.join("sb1.p0.bin")).expect("part"),
            b"INITHEADTAILNEXT"
        );
    }

    #[test]
    fn split_init_is_one_init_and_its_duplicate_is_dropped_whole() {
        let mut s = session("splitinit");
        write(&mut s, &seg(1, true, None, None), b"IN");
        write(&mut s, &seg(1, false, None, None), b"IT");
        write(&mut s, &seg(1, false, Some(0), None), b"M0");
        // Seek back: the player appends both init chunks again.
        write(&mut s, &seg(1, true, None, None), b"IN");
        write(&mut s, &seg(1, false, None, None), b"IT");
        write(&mut s, &seg(1, false, Some(1000), None), b"M1");
        assert_eq!(
            fs::read(s.dir.join("sb1.p0.bin")).expect("part"),
            b"INITM0M1"
        );
        let rows: Vec<serde_json::Value> = fs::read_to_string(s.dir.join("index.jsonl"))
            .expect("index")
            .lines()
            .map(|l| serde_json::from_str(l).expect("json"))
            .collect();
        assert_eq!(rows[1]["is_init"], true, "the continuation is init bytes");
    }

    #[test]
    fn sequence_mode_dedupes_by_tfdt_and_duration() {
        let mut s = session("seqmode");
        write(&mut s, &seg(1, true, None, None), b"INIT");
        let mut h = seg(1, false, Some(0), Some(3.0));
        h.seq_mode = true;
        write(&mut s, &h, b"A");
        h.tso = Some(7.0); // rewritten by the browser, not a new period
        write(&mut s, &h, b"A");
        assert_eq!(
            index_lines(&s.dir),
            2,
            "a re-append after a seek back is a duplicate"
        );
    }

    #[test]
    fn switching_back_to_an_earlier_init_continues_its_part() {
        let mut s = session("abr");
        write(&mut s, &seg(1, true, None, None), b"LOW-");
        write(&mut s, &seg(1, false, Some(0), None), b"l0");
        write(&mut s, &seg(1, true, None, None), b"HIGH");
        write(&mut s, &seg(1, false, Some(1000), None), b"h1");
        write(&mut s, &seg(1, true, None, None), b"LOW-");
        write(&mut s, &seg(1, false, Some(2000), None), b"l2");
        assert_eq!(fs::read(s.dir.join("sb1.p0.bin")).expect("p0"), b"LOW-l0l2");
        assert_eq!(fs::read(s.dir.join("sb1.p1.bin")).expect("p1"), b"HIGHh1");
        assert!(!s.dir.join("sb1.p2.bin").exists(), "no third part");
    }

    #[test]
    fn media_before_the_first_init_gets_its_own_part() {
        let mut s = session("preinit");
        write(&mut s, &seg(1, false, Some(0), None), b"OLD");
        write(&mut s, &seg(1, true, None, None), b"INIT");
        write(&mut s, &seg(1, false, Some(1000), None), b"NEW");
        assert_eq!(fs::read(s.dir.join("sb1.p0.bin")).expect("p0"), b"OLD");
        assert_eq!(fs::read(s.dir.join("sb1.p1.bin")).expect("p1"), b"INITNEW");
    }

    #[test]
    fn encrypted_track_is_not_written() {
        let mut s = session("drm");
        write(&mut s, &seg(1, true, None, None), b"....encv....");
        write(&mut s, &seg(1, false, Some(0), None), b"CIPHER");
        assert_eq!(index_lines(&s.dir), 0);
        assert_eq!(fs::read(s.dir.join("sb1.p0.bin")).expect("part").len(), 0);
        // A clear init afterwards is captured again.
        write(&mut s, &seg(1, true, None, None), b"CLEARINIT");
        write(&mut s, &seg(1, false, Some(0), None), b"MEDIA");
        assert_eq!(index_lines(&s.dir), 2);
    }

    #[test]
    fn encrypted_continuation_of_a_split_init_is_refused() {
        let mut s = session("drmsplit");
        write(&mut s, &seg(1, true, None, None), b"\0\0\0\x08ftyp");
        write(&mut s, &seg(1, false, None, None), b"..encv..");
        write(&mut s, &seg(1, false, Some(0), None), b"CIPHER");
        assert_eq!(
            index_lines(&s.dir),
            0,
            "a split init is decided whole: nothing written"
        );
    }

    #[test]
    fn piff_sample_encryption_is_refused() {
        let mut uuid = PIFF_SAMPLE_ENCRYPTION.to_vec();
        uuid.extend_from_slice(b"rest");
        let piff = mp4_box(*b"moof", &mp4_box(*b"traf", &mp4_box(*b"uuid", &uuid)));
        assert!(payload_encrypted(
            &seg(1, false, Some(0), None),
            &piff,
            false,
            "mp4"
        ));
    }

    #[test]
    fn session_budget_counts_index_lines_and_stops_for_good() {
        let out = temp_dir("cap");
        let mut s = Session::open(&out, "test", "u", "t", budget(9000, 64 * GIB))
            .expect("session")
            .expect("within budget");
        write(&mut s, &seg(1, true, None, None), b"INIT");
        write(&mut s, &seg(1, false, Some(0), None), b"");
        write(&mut s, &seg(1, false, Some(1), None), b"X");
        assert_eq!(index_lines(&s.dir), 2, "an empty payload is not written");
        for t in 1..20 {
            write(&mut s, &seg(1, false, Some(t), None), b"X");
        }
        let lines = index_lines(&s.dir);
        assert!(
            lines < 20,
            "index lines count against the cap ({lines} written)"
        );
        assert!(s.stopped);
        let before = index_lines(&s.dir);
        write(&mut s, &seg(1, false, Some(999), None), b"Y");
        assert_eq!(index_lines(&s.dir), before, "stopped stays stopped");
    }

    #[test]
    fn process_budget_is_shared_by_sessions() {
        // Two directories and two part files (about 4 KiB each) fit, then a
        // few rows.
        let shared = budget(16 * GIB, 20_000);
        let open = |name: &str| {
            let dir = temp_dir(name);
            let s = Session::open(&dir, name, "u", "t", Arc::clone(&shared))
                .expect("session")
                .expect("within budget");
            TestSession { s, _dir: dir }
        };
        let mut a = open("tot-a");
        let mut b = open("tot-b");
        write(&mut a, &seg(1, true, None, None), b"INIT");
        write(&mut b, &seg(1, true, None, None), b"INIT");
        for t in 0..10 {
            write(&mut a, &seg(1, false, Some(t), None), b"X");
            write(&mut b, &seg(1, false, Some(t), None), b"X");
        }
        assert!(shared.total_used.load(Ordering::Relaxed) <= 20_000);
        assert!(a.stopped || b.stopped);
        // A spent process budget opens no further directory.
        let out = temp_dir("tot-c");
        let c = Session::open(&out, "c", "u", "t", Arc::clone(&shared)).expect("no io error");
        assert!(c.is_none());
        assert_eq!(
            fs::read_dir(&out).expect("dir").count(),
            0,
            "nothing created"
        );
    }

    #[test]
    fn combined_init_and_media_appends_switch_back_into_their_part() {
        // video.js VHS: the init is appended together with a segment.
        let mut s = session("vhs");
        let init_a = init_with_traks(1);
        let mut init_b = init_with_traks(1);
        init_b.extend(mp4_box(*b"free", b"B"));
        let with = |init: &[u8], t: u32| {
            let mut v = init.to_vec();
            v.extend(moof(t));
            v
        };
        write(&mut s, &seg(1, true, Some(0), None), &with(&init_a, 0));
        write(&mut s, &seg(1, false, Some(1000), None), &moof(1000));
        write(
            &mut s,
            &seg(1, true, Some(2000), None),
            &with(&init_b, 2000),
        );
        write(
            &mut s,
            &seg(1, true, Some(3000), None),
            &with(&init_a, 3000),
        );
        write(&mut s, &seg(1, false, Some(4000), None), &moof(4000));
        s.finish();
        let p0 = fs::read(s.dir.join("sb1.p0.bin")).expect("p0");
        assert_eq!(
            p0.len(),
            init_a.len() + 4 * 32,
            "A init once, A media 0, 1, 3, 4"
        );
        assert!(
            !s.dir.join("sb1.p2.bin").exists(),
            "the switch back made no third part"
        );
    }

    #[test]
    fn split_inits_are_compared_whole() {
        let mut s = session("splitdiff");
        // Same first chunk, different rest: two renditions, two parts.
        write(&mut s, &seg(1, true, None, None), b"HEAD");
        write(&mut s, &seg(1, false, None, None), b"-A");
        write(&mut s, &seg(1, false, Some(0), None), b"a0");
        write(&mut s, &seg(1, true, None, None), b"HEAD");
        write(&mut s, &seg(1, false, None, None), b"-B");
        write(&mut s, &seg(1, false, Some(1000), None), b"b1");
        assert_eq!(fs::read(s.dir.join("sb1.p0.bin")).expect("p0"), b"HEAD-Aa0");
        assert_eq!(fs::read(s.dir.join("sb1.p1.bin")).expect("p1"), b"HEAD-Bb1");
    }

    #[test]
    fn sequence_mode_keeps_a_new_period_with_reused_tfdt() {
        let mut s = session("seqperiod");
        write(&mut s, &seg(1, true, None, None), b"INIT");
        let mut h = seg(1, false, Some(0), None);
        h.seq_mode = true;
        write(&mut s, &h, b"period-1");
        write(&mut s, &h, b"period-2"); // discontinuity: tfdt restarts
        write(&mut s, &h, b"period-1"); // seek back: same bytes
        assert_eq!(
            index_lines(&s.dir),
            3,
            "init, both periods; the re-append dropped"
        );
    }

    #[test]
    fn media_bytes_never_count_as_init_markers() {
        let mut s = session("nofalsedrm");
        write(&mut s, &seg(1, true, None, None), &init_with_traks(1));
        write(&mut s, &seg(1, false, Some(0), None), &moof(0));
        // An untimed media continuation that happens to hold "encv".
        write(&mut s, &seg(1, false, None, None), b"xxxxencvxxxx");
        write(&mut s, &seg(1, false, Some(1000), None), &moof(1000));
        assert_eq!(index_lines(&s.dir), 4, "nothing refused");
        assert!(!s.tracks[&1].refused);
    }

    #[test]
    fn untimed_media_of_a_combined_append_is_not_init_bytes() {
        // Init and a segment in one append, the moof without a tfdt: the
        // media part is not a continuation of the init, so its mdat bytes
        // are not scanned for init markers and not hashed into the init.
        let mut s = session("vhsnotfdt");
        let mut append = init_with_traks(1);
        append.extend(mp4_box(*b"moof", &mp4_box(*b"traf", b"")));
        append.extend(mp4_box(*b"mdat", b"xxxxencvxxxx"));
        write(&mut s, &seg(1, true, None, None), &append);
        write(&mut s, &seg(1, false, Some(1000), None), &moof(1000));
        assert!(!s.tracks[&1].refused, "no false DRM from mdat bytes");
        let rows: Vec<serde_json::Value> = fs::read_to_string(s.dir.join("index.jsonl"))
            .expect("index")
            .lines()
            .map(|l| serde_json::from_str(l).expect("json"))
            .collect();
        let inits: Vec<bool> = rows.iter().map(|r| r["is_init"] == true).collect();
        assert_eq!(
            inits,
            [true, false, false],
            "init row, media row, media row"
        );
    }

    #[test]
    fn init_prefix_is_found_in_mp4_and_webm() {
        let init = init_with_traks(1);
        let mut combined = init.clone();
        combined.extend(moof(0));
        assert_eq!(init_prefix_len("mp4", &combined), Some(init.len()));
        assert_eq!(init_prefix_len("mp4", &init), None);
        // EBML header, Segment (unknown size) > Info, Cluster.
        let webm = [
            0x1a, 0x45, 0xdf, 0xa3, 0x81, 0x00, 0x18, 0x53, 0x80, 0x67, 0x01, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0x15, 0x49, 0xa9, 0x66, 0x80, 0x1f, 0x43, 0xb6, 0x75, 0x80,
        ];
        assert_eq!(init_prefix_len("webm", &webm), Some(23));
    }

    #[test]
    fn multi_track_segments_shift_each_traf_by_its_timescale() {
        // tkhd v0: fullbox(4) ctime(4) mtime(4) track_ID(4); mdhd v0: fullbox, ctime, mtime, timescale.
        let trak = |id: u32, ts: u32| {
            let mut tkhd = vec![0u8; 16];
            tkhd[12..16].copy_from_slice(&id.to_be_bytes());
            let mut mdhd = vec![0u8; 16];
            mdhd[12..16].copy_from_slice(&ts.to_be_bytes());
            mp4_box(
                *b"trak",
                &[
                    mp4_box(*b"tkhd", &tkhd),
                    mp4_box(*b"mdia", &mp4_box(*b"mdhd", &mdhd)),
                ]
                .concat(),
            )
        };
        let mut init = mp4_box(*b"ftyp", b"iso6");
        init.extend(mp4_box(
            *b"moov",
            &[trak(1, 90_000), trak(2, 48_000)].concat(),
        ));
        let ts = trak_timescales(&init);
        assert_eq!(ts.get(&1), Some(&90_000));
        assert_eq!(ts.get(&2), Some(&48_000));
        let frag = |id: u32| {
            let mut tfhd = vec![0u8; 8];
            tfhd[4..8].copy_from_slice(&id.to_be_bytes());
            mp4_box(
                *b"traf",
                &[mp4_box(*b"tfhd", &tfhd), mp4_box(*b"tfdt", &[0u8; 8])].concat(),
            )
        };
        let mut seg = mp4_box(*b"moof", &[frag(1), frag(2)].concat());
        let by_track =
            |tid: Option<u32>| shift_ticks(Some(10.0), tid.and_then(|t| ts.get(&t)).copied());
        assert!(shift_tfdt_by(&mut seg, &by_track));
        let tfdt_at = |n: usize| {
            let at = seg
                .windows(4)
                .enumerate()
                .filter(|(_, w)| w == b"tfdt")
                .nth(n)
                .expect("tfdt")
                .0
                + 8;
            u32::from_be_bytes([seg[at], seg[at + 1], seg[at + 2], seg[at + 3]])
        };
        assert_eq!(tfdt_at(0), 900_000, "video: 10 s at 90 kHz");
        assert_eq!(tfdt_at(1), 480_000, "audio: 10 s at 48 kHz");
    }

    #[test]
    fn a_part_with_a_cut_init_is_left_out() {
        let dir = temp_dir("cutinit");
        let full = init_with_traks(1);
        let cut = &full[..full.len() - 4];
        fs::write(dir.join("sb1.p0.bin"), cut).expect("write");
        let r = row(0, cut.len() as u64, true, None, None);
        assert!(
            matches!(
                mux_inputs(&dir, &[r], &Tools::default()),
                Err(SinkError::Capture(_))
            ),
            "no complete moov: skipped"
        );
    }

    fn tools_without(tool: Tool) -> Tools {
        let mut tools = Tools::default();
        let missing = std::ffi::OsString::from("streamtap-no-such-tool-4711");
        match tool {
            Tool::Ffmpeg => tools.ffmpeg = missing,
            Tool::Ffprobe => tools.ffprobe = missing,
        }
        tools
    }

    /// ENG-13: a missing ffprobe is said once, the parts are kept with the
    /// capture's own start instead of silently counting as "start unknown".
    #[test]
    fn a_missing_ffprobe_is_reported_not_swallowed() {
        let warnings = plan_two_parts_with("noprobe", &tools_without(Tool::Ffprobe));
        assert!(warnings[0].starts_with("ffprobe not found"), "{warnings:?}");
    }

    /// ENG-13 also for an ffprobe that exists but cannot be started: one
    /// warning, not one (and one spawn attempt) per part.
    #[test]
    fn an_unstartable_ffprobe_is_reported_once() {
        let dir = temp_dir("badprobe");
        let bad = dir.join("ffprobe-not-a-program.exe");
        fs::write(&bad, "this is a text file, not a program").expect("write");
        let tools = Tools {
            ffprobe: bad.into(),
            ..Tools::default()
        };
        let warnings = plan_two_parts_with("badprobe-parts", &tools);
        assert!(
            warnings[0].starts_with("cannot run ffprobe"),
            "{warnings:?}"
        );
    }

    /// Plans two MP4 parts with `tools` (ffprobe unusable); expects both kept
    /// with the capture's start and exactly one warning, which it returns.
    fn plan_two_parts_with(name: &str, tools: &Tools) -> Vec<String> {
        let dir = temp_dir(name);
        let init = init_with_traks(1);
        let mut seg = init.clone();
        seg.extend(moof(2000));
        let mut rows = Vec::new();
        for sb in [1, 2] {
            fs::write(dir.join(format!("sb{sb}.p0.bin")), &seg).expect("write");
            let mut i = row(0, init.len() as u64, true, None, None);
            let mut m = row(
                init.len() as u64,
                (seg.len() - init.len()) as u64,
                false,
                Some(2000),
                None,
            );
            i.sb = sb;
            m.sb = sb;
            rows.push(i);
            rows.push(m);
        }
        let plan = mux_inputs(&dir, &rows, tools).expect("plan");
        assert_eq!(plan.inputs.len(), 2, "parts are kept: {plan:?}");
        assert!(
            plan.inputs.iter().all(|i| i.start == Some(2.0)),
            "the capture's start is used: {plan:?}"
        );
        assert_eq!(plan.warnings.len(), 1, "said once: {:?}", plan.warnings);
        plan.warnings
    }

    #[test]
    fn the_part_file_keeps_the_extension() {
        assert_eq!(
            part_path(Path::new("cap/out.mp4")),
            Path::new("cap/out.part.mp4")
        );
        assert_eq!(part_path(Path::new("cap/out")), Path::new("cap/out.part"));
    }

    /// A mux killed at its deadline (or the output limit) leaves neither a
    /// truncated `out` nor its part file, and an older `out` stays intact.
    #[test]
    fn a_killed_mux_leaves_no_truncated_output() {
        let dir = temp_dir("killedmux");
        let out = dir.join("out.mp4");
        fs::write(&out, b"older result").expect("write");
        let err = write_through_part(&out, |part| -> Result<(), SinkError> {
            fs::write(part, b"truncated, no moov").expect("write part");
            Err(SinkError::ToolTimeout {
                tool: Tool::Ffmpeg,
                after: Duration::from_secs(1),
            })
        })
        .unwrap_err();
        assert_eq!(err.exit_code(), 5);
        assert_eq!(fs::read(&out).expect("out"), b"older result");
        assert!(!part_path(&out).exists(), "part file removed");
    }

    #[test]
    fn a_finished_mux_replaces_the_output() {
        let dir = temp_dir("finishedmux");
        let out = dir.join("out.mp4");
        fs::write(&out, b"older result").expect("write");
        write_through_part(&out, |part| {
            fs::write(part, b"new result").expect("write part");
            Ok(())
        })
        .expect("mux");
        assert_eq!(fs::read(&out).expect("out"), b"new result");
        assert!(!part_path(&out).exists());
    }

    #[test]
    fn a_missing_ffmpeg_fails_the_mux_with_its_own_code() {
        let dir = temp_dir("noffmpeg");
        fs::write(dir.join("index.jsonl"), "").expect("write");
        let args = [dir.to_string_lossy().into_owned()];
        let err = run_mux(&args, &tools_without(Tool::Ffmpeg)).unwrap_err();
        assert!(
            matches!(err, SinkError::ToolMissing { tool: Tool::Ffmpeg }),
            "{err}"
        );
        assert_eq!(err.exit_code(), 4);
    }

    #[test]
    fn mux_usage_errors_exit_with_2() {
        for args in [
            vec![],
            vec![
                "dir".to_owned(),
                "--timeout-secs".to_owned(),
                "0".to_owned(),
            ],
        ] {
            let err = run_mux(&args, &Tools::default()).unwrap_err();
            assert_eq!(err.exit_code(), 2, "{err}");
        }
    }

    #[test]
    fn version_line_drops_the_copyright() {
        assert_eq!(
            version_line("ffmpeg version 7.1-full_build-www.gyan.dev Copyright (c) 2000-2024 the FFmpeg developers
built with gcc"),
            "ffmpeg version 7.1-full_build-www.gyan.dev"
        );
        assert!(version_line("").contains("unknown"));
    }

    #[test]
    fn ffmpeg_runs_quiet_and_maps_every_input() {
        let inputs = [MuxInput {
            path: PathBuf::from("v"),
            format: "mp4",
            start: None,
        }];
        let args: Vec<String> = ffmpeg_args(&inputs, Path::new("out.mp4"))
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args
            .windows(2)
            .any(|w| w[0] == "-loglevel" && w[1] == "error"));
        assert!(args.iter().any(|a| a == "-nostats"));
        assert!(args.windows(2).any(|w| w[0] == "-map" && w[1] == "0"));
        assert_eq!(args.last().map(String::as_str), Some("out.mp4"));
    }

    #[test]
    fn printable_strips_terminal_escapes() {
        assert_eq!(
            printable("video/mp4\x1b]0;pwned\x07", 64),
            "video/mp4?]0;pwned?"
        );
        assert_eq!(printable("abc", 2), "ab");
    }

    #[test]
    fn track_count_is_bounded() {
        let mut s = session("tracks");
        let limit = u32::try_from(MAX_TRACKS).expect("small constant");
        for sb in 0..(limit + 10) {
            write(&mut s, &seg(sb, true, None, None), b"I");
        }
        assert_eq!(s.tracks.len(), MAX_TRACKS);
    }

    #[test]
    fn sessions_never_share_a_directory() {
        let out = temp_dir("dirs");
        let a = Session::open(&out, "same", "u", "t", budget(GIB, GIB))
            .expect("a")
            .expect("within budget");
        let b = Session::open(&out, "same", "u", "t", budget(GIB, GIB))
            .expect("b")
            .expect("within budget");
        assert_ne!(a.dir, b.dir);
        assert!(a.dir.join("session.json").is_file());
    }

    #[test]
    fn per_segment_boxes_are_not_init_bytes() {
        // DASH packagers put styp and a per-segment sidx in front of the
        // moof; with the init in the same append they belong to the media.
        let init = init_with_traks(1);
        let mut append = init.clone();
        append.extend(mp4_box(*b"styp", b"msdh"));
        append.extend(mp4_box(*b"sidx", b"1234"));
        append.extend(moof(0));
        assert_eq!(init_prefix_len("mp4", &append), Some(init.len()));
        let mut free = init.clone();
        free.extend(mp4_box(*b"free", b"x"));
        let mut with_free = free.clone();
        with_free.extend(moof(0));
        assert_eq!(
            init_prefix_len("mp4", &with_free),
            Some(free.len()),
            "a free box after moov stays init"
        );

        // A switch back with a different sidx each time continues part 0.
        let mut s = session("sidx");
        let init_b = init_with_traks(2);
        let with = |init: &[u8], t: u32| {
            let mut v = init.to_vec();
            v.extend(mp4_box(*b"styp", b"msdh"));
            v.extend(mp4_box(*b"sidx", &t.to_be_bytes()));
            v.extend(moof(t));
            v
        };
        write(&mut s, &seg(1, true, Some(0), None), &with(&init, 0));
        write(
            &mut s,
            &seg(1, true, Some(1000), None),
            &with(&init_b, 1000),
        );
        write(&mut s, &seg(1, true, Some(2000), None), &with(&init, 2000));
        s.finish();
        assert!(!s.dir.join("sb1.p2.bin").exists(), "no third part");
        assert_eq!(s.tracks[&1].part, 0, "back in part 0");
    }

    #[test]
    fn untimed_media_after_a_complete_init_is_media() {
        // The init alone, then a segment cut inside its moof (the first
        // piece has no tfdt): the piece is media, so the same init after a
        // seek is a duplicate and opens no part.
        let mut s = session("cutmoof");
        let init = init_with_traks(1);
        for t in [0u32, 1000] {
            write(&mut s, &seg(1, true, None, None), &init);
            let m = moof(t);
            write(&mut s, &seg(1, false, None, None), &m[..10]);
            write(&mut s, &seg(1, false, None, None), &m[10..]);
        }
        s.finish();
        assert!(!s.dir.join("sb1.p1.bin").exists(), "one part");
        let rows: Vec<serde_json::Value> = fs::read_to_string(s.dir.join("index.jsonl"))
            .expect("index")
            .lines()
            .map(|l| serde_json::from_str(l).expect("json"))
            .collect();
        let inits = rows.iter().filter(|r| r["is_init"] == true).count();
        assert_eq!(inits, 1, "one init row, the pieces are media");
    }

    #[test]
    fn init_complete_needs_moov_or_tracks() {
        let init = init_with_traks(1);
        assert!(init_complete("mp4", &init));
        assert!(!init_complete("mp4", &init[..init.len() - 1]));
        assert!(!init_complete("mp4", &mp4_box(*b"ftyp", b"iso6")));
        // EBML header, Segment (unknown size), Info, Tracks.
        let ebml = [0x1a, 0x45, 0xdf, 0xa3, 0x80];
        let segment = [
            0x18, 0x53, 0x80, 0x67, 0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        ];
        let info = [0x15, 0x49, 0xa9, 0x66, 0x80];
        let tracks = [0x16, 0x54, 0xae, 0x6b, 0x80];
        let mut head = [&ebml[..], &segment, &info].concat();
        assert!(!init_complete("webm", &head));
        head.extend_from_slice(&tracks);
        assert!(init_complete("webm", &head));
    }

    #[test]
    fn parts_per_track_are_capped() {
        let mut s = session("parts");
        for i in 0..(MAX_PARTS_PER_TRACK + 6) {
            let mut init = init_with_traks(1);
            init.extend(mp4_box(*b"free", &i.to_be_bytes()));
            write(&mut s, &seg(1, true, None, None), &init);
            write(&mut s, &seg(1, false, Some(u64::from(i)), None), &moof(i));
        }
        s.finish();
        let files = fs::read_dir(&s.dir)
            .expect("dir")
            .filter(|e| {
                e.as_ref()
                    .is_ok_and(|e| e.file_name().to_string_lossy().starts_with("sb1.p"))
            })
            .count();
        assert_eq!(files, MAX_PARTS_PER_TRACK as usize);
        assert!(s.tracks[&1].parts_full);
        assert!(s.tracks[&1].skipped >= 6, "media under new inits skipped");
    }

    #[test]
    fn sequence_mode_lays_out_jumps_like_the_browser() {
        // Period 1 (0, 1000), then period 2 restarting tfdt (0, 1000), then a
        // seek back fetching 500 again: placed one after the other.
        let dir = temp_dir("seqlayout");
        let init = init_with_traks(1);
        let segs = [moof(0), moof(1000), moof(0), moof(1000), moof(500)];
        let mut rows = write_part(
            &dir,
            &[
                (&init, true, None, None),
                (&segs[0], false, Some(0), None),
                (&segs[1], false, Some(1000), None),
                (&segs[2], false, Some(0), None),
                (&segs[3], false, Some(1000), None),
                (&segs[4], false, Some(500), None),
            ],
        );
        for r in &mut rows[1..] {
            r.seq_mode = true;
            r.dur = Some(1000);
        }
        let got = order(&dir, &rows).0;
        let media = &got[init.len()..];
        let tfdts: Vec<u32> = media.chunks(32).map(tfdt_of).collect();
        assert_eq!(tfdts, [0, 1000, 2000, 3000, 4000]);

        // Without durations the jumps cannot be laid out: arrival order.
        for r in &mut rows[1..] {
            r.dur = None;
        }
        let (got, path, _) = order(&dir, &rows);
        assert_eq!(path, dir.join("sb1.p0.bin"), "unchanged");
        let tfdts: Vec<u32> = got[init.len()..].chunks(32).map(tfdt_of).collect();
        assert_eq!(tfdts, [0, 1000, 0, 1000, 500]);
    }

    #[test]
    fn sequence_mode_follows_a_set_timestamp_offset() {
        // Shaka in sequence mode: 0, 1 s, a seek to 4 s (timestampOffset set
        // to 4), 5 s (the browser left the offset at 0), a seek back to 2 s
        // (set to 2), 3 s. The browser places each at its own time.
        let dir = temp_dir("seqtso");
        let init = init_with_traks(1);
        let segs = [
            moof(0),
            moof(1000),
            moof(4000),
            moof(5000),
            moof(2000),
            moof(3000),
        ];
        let set = [false, false, true, false, true, false];
        let tso = [0.0, 0.0, 4.0, 0.0, 2.0, 0.0];
        let mut list: Vec<TestSeg<'_>> = vec![(&init, true, None, None)];
        for (i, s) in segs.iter().enumerate() {
            list.push((s, false, Some(u64::from(tfdt_of(s))), Some(tso[i])));
        }
        let mut rows = write_part(&dir, &list);
        for (i, r) in rows[1..].iter_mut().enumerate() {
            r.seq_mode = true;
            r.dur = Some(1000);
            r.tso_set = set[i];
        }
        let got = order(&dir, &rows).0;
        let tfdts: Vec<u32> = got[init.len()..].chunks(32).map(tfdt_of).collect();
        assert_eq!(tfdts, [0, 1000, 2000, 3000, 4000, 5000], "in time order");
        let refs: Vec<&IndexRow> = rows.iter().collect();
        let (moves, jumps) = seq_layout(&refs, &HashMap::new());
        assert_eq!(jumps, 0, "no discontinuity");
        assert!(
            moves.values().all(|m| *m == 0),
            "each at its own time, none moved: {moves:?}"
        );
    }

    #[test]
    fn sequence_mode_layout_spans_all_parts() {
        // An ABR switch and back: part 0 has 0, 1 s and 4, 5 s, part 1 the
        // 2, 3 s in between. One timeline: nothing moves.
        let init = init_with_traks(1);
        let mut rows = Vec::new();
        for (i, (part, t)) in [
            (0, 0),
            (0, 1000),
            (1, 2000),
            (1, 3000),
            (0, 4000),
            (0, 5000),
        ]
        .into_iter()
        .enumerate()
        {
            let mut r = row(i as u64 * 100, 32, false, Some(t), Some(0.0));
            r.part = part;
            r.seq_mode = true;
            r.dur = Some(1000);
            rows.push(r);
        }
        let mut first = row(9999, init.len() as u64, true, None, None);
        first.seq_mode = true;
        rows.insert(0, first);
        let refs: Vec<&IndexRow> = rows.iter().collect();
        let (moves, jumps) = seq_layout(&refs, &HashMap::new());
        assert_eq!(jumps, 0);
        assert!(moves.values().all(|m| *m == 0), "{moves:?}");
    }

    #[test]
    fn init_index_range_stays_one_init() {
        // SegmentBase: ftyp moov sidx with no moof is all init.
        let mut v = init_with_traks(1);
        v.extend(mp4_box(*b"sidx", b"1234"));
        assert_eq!(init_prefix_len("mp4", &v), None);
    }

    #[test]
    fn init_scan_resumes_where_it_stopped() {
        let init = init_with_traks(1);
        let (done, at) = init_scan("mp4", &init[..20], 0);
        assert!(!done);
        assert_eq!(at, 12, "after the complete ftyp");
        assert!(init_scan("mp4", &init, at).0);
    }

    #[test]
    fn a_set_offset_places_the_first_presentation_time() {
        // trun with duration and composition offset: the first frame shows
        // 80 ticks after its decode time.
        let mut trun = vec![0, 0, 0x09, 0x00]; // v0, duration + cto
        trun.extend(1u32.to_be_bytes());
        trun.extend(40u32.to_be_bytes());
        trun.extend(80u32.to_be_bytes());
        let seg = mp4_box(*b"moof", &mp4_box(*b"traf", &mp4_box(*b"trun", &trun)));
        assert_eq!(first_cto(&seg), 80);
        assert_eq!(first_cto(&moof(0)), 0, "no trun");

        let mut r = row(0, 32, false, Some(1000), Some(4.0));
        r.seq_mode = true;
        r.tso_set = true;
        r.dur = Some(1000);
        let leads: SeqMoves = HashMap::from([((0, 0), 80)]);
        let (moves, _) = seq_layout(&[&r], &leads);
        // PTS (1000 + 80) at 4 s: tfdt goes to 3920.
        assert_eq!(moves[&(0, 0)], 2920);
    }

    #[test]
    fn a_cut_moof_still_splits_a_combined_append() {
        let init = init_with_traks(1);
        let mut v = init.clone();
        v.extend(mp4_box(*b"styp", b"msdh"));
        v.extend(&moof(0)[..12]); // header says 40 bytes, 12 arrived
        assert_eq!(init_prefix_len("mp4", &v), Some(init.len()));
    }

    #[test]
    fn pending_chunks_count_their_header() {
        // An ftyp-only init, then 1-byte chunks: each costs its header too.
        let mut s = session("pendcost");
        write(
            &mut s,
            &seg(1, true, None, None),
            &mp4_box(*b"ftyp", b"iso6"),
        );
        for _ in 0..(MAX_PENDING_INIT / PENDING_CHUNK_COST + 10) {
            write(&mut s, &seg(1, false, None, None), b"x");
        }
        assert!(s.tracks[&1].pending_cost() <= MAX_PENDING_INIT);
    }

    #[test]
    fn rescale_rounds_between_timescales() {
        assert_eq!(rescale(1000, Some(1000), 90_000), 90_000);
        assert_eq!(rescale(-3, Some(2), 1), -2);
        assert_eq!(rescale(5, None, 1000), 0);
    }
}

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
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

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
    let code = match args.first().map(String::as_str) {
        Some("serve") => run_serve(&args[1..]),
        Some("mux") => run_mux(&args[1..]),
        _ => {
            eprintln!(
                "usage:\n  streamtap-sink serve --token <token> [--port {DEFAULT_PORT}] [--out captures] [--allow-origin {DEFAULT_ORIGIN}]\n  streamtap-sink mux <capture-dir> [--out <file.mp4>]"
            );
            2
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

fn run_serve(args: &[String]) -> i32 {
    let Some(token) = flag(args, "--token") else {
        eprintln!("serve: --token is required (the extension must send the same value)");
        return 2;
    };
    let port = match flag(args, "--port").map(str::parse::<u16>) {
        None => DEFAULT_PORT,
        Some(Ok(p)) => p,
        Some(Err(_)) => {
            eprintln!("serve: --port must be a number");
            return 2;
        }
    };
    let out = PathBuf::from(flag(args, "--out").unwrap_or("captures"));
    let origin = flag(args, "--allow-origin")
        .unwrap_or(DEFAULT_ORIGIN)
        .to_owned();
    let gib = |name: &str, default: u64| match flag(args, name).map(str::parse::<u64>) {
        None => Some(default << 30),
        Some(Ok(g)) if (1..=4096).contains(&g) => Some(g << 30),
        Some(_) => {
            eprintln!("serve: {name} must be a whole number from 1 to 4096");
            None
        }
    };
    let (Some(per_session), Some(total_max)) = (
        gib("--max-session-gib", DEFAULT_MAX_SESSION_GIB),
        gib("--max-total-gib", DEFAULT_MAX_TOTAL_GIB),
    ) else {
        return 2;
    };
    let budget = Arc::new(Budget {
        per_session,
        total_max,
        total_used: AtomicU64::new(0),
    });
    if let Err(e) = fs::create_dir_all(&out) {
        eprintln!("serve: cannot create {}: {e}", out.display());
        return 1;
    }

    // Loopback only. Binding anything else is not an option, not a flag.
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("serve: bind 127.0.0.1:{port} failed: {e}");
            return 1;
        }
    };
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
    0
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
    /// one after the other and rewrites `timestampOffset` itself, so the
    /// recorded offset is not used.
    #[serde(default, rename = "seqMode")]
    seq_mode: bool,
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
    mime: &'a str,
    kind: &'a str,
}

/// Media segments remembered for dedupe, per session (all parts and tracks
/// together): beyond this the session takes no more media.
const MAX_SEEN_PER_SESSION: usize = 400_000;
/// Init bytes held until the init is complete (an init split across appends).
const MAX_PENDING_INIT: usize = 4 * 1024 * 1024;

/// Dedupe keys: (tfdt, timestampOffset in ms, duration), or in sequence mode
/// (payload hash, `i64::MIN`, length).
type SeenSet = HashSet<(u64, i64, u64)>;

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
    pending: Vec<(SegmentHeader, Vec<u8>)>,
    pending_bytes: usize,
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

/// Where the init part of an init+media append ends (video.js VHS appends the
/// init together with the first segment): the first top-level `moof`/`mdat`
/// (MP4), or the first Cluster (`WebM`). `None` when there is no media in it.
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
    let mut start = 0;
    for (kind, _, end) in boxes_in(payload, 0, payload.len()) {
        if &kind == b"moof" || &kind == b"mdat" {
            return (start > 0).then_some(start);
        }
        start = end;
    }
    None
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
                if budget.total_used.load(Ordering::Relaxed) >= budget.total_max {
                    eprintln!("{peer}: --max-total-gib reached, closing");
                    break;
                }
                let s = Session::open(out, &id, &url, &title, Arc::clone(budget))?;
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
            pending_bytes: 0,
            pending_after_refusal: false,
            init_kind: String::new(),
            skip_tail: false,
            refused: false,
            bytes: 0,
            segments: 0,
            dupes: 0,
            skipped: 0,
        }
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
            println!("sb{sb} init seen before, continuing part {earlier}");
            self.dupes += 1;
            return Ok(false);
        }
        // Media that came before the first init (the hook attached late)
        // stays alone in its part.
        if !self.hash_part.is_empty() || self.offset > 0 {
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
    ) -> io::Result<Self> {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
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
        let meta = serde_json::json!({ "url": url, "title": title, "session": session_id, "started": secs });
        let meta = serde_json::to_vec_pretty(&meta)?;
        // Written atomically: a reader never sees half a file.
        let tmp = dir.join("session.json.tmp");
        fs::write(&tmp, &meta)?;
        fs::rename(&tmp, dir.join("session.json"))?;
        let index = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("index.jsonl"))?;
        let mut s = Self {
            dir,
            index,
            tracks: HashMap::new(),
            track_limit_logged: false,
            bytes_total: 0,
            budget,
            stopped: false,
            seen_total: 0,
            seen_limit_logged: false,
        };
        // The directory and session.json count against the budget too.
        s.within_budget(meta.len() as u64 + 4096);
        Ok(s)
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
        let chunks = std::mem::take(&mut track.pending);
        track.pending_bytes = 0;
        let mut whole = Vec::new();
        for (_, c) in &chunks {
            whole.extend_from_slice(c);
        }
        if !track.decide_init(fnv1a(&whole), &dir, sb)? {
            return Ok(());
        }
        for (h, c) in &chunks {
            self.write_row(sb, h, c, true)?;
        }
        Ok(())
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
        if !self.tracks.contains_key(&h.sb) {
            if self.tracks.len() >= MAX_TRACKS {
                if !self.track_limit_logged {
                    eprintln!(
                        "{}: more than {MAX_TRACKS} SourceBuffers, ignoring sb{} and further new ones",
                        dir.display(),
                        h.sb
                    );
                    self.track_limit_logged = true;
                }
                return Ok(());
            }
            let file = Self::open_part(&dir, h.sb, 0)?;
            self.tracks.insert(h.sb, TrackState::new(file));
        }
        let track = self.tracks.get_mut(&h.sb).expect("present");
        let continuation =
            !h.is_init && h.tfdt.is_none() && (h.init_cont || !track.pending.is_empty());

        if payload_encrypted(h, payload, continuation, &track.init_kind) {
            if !track.refused {
                eprintln!(
                    "{}: sb{} carries encrypted media (DRM); not capturing this track",
                    dir.display(),
                    h.sb
                );
            }
            track.refused = true;
            track.pending.clear();
            track.pending_bytes = 0;
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
            track.pending.push((h.clone(), payload.to_vec()));
            track.pending_bytes = payload.len();
            if track.pending_bytes > MAX_PENDING_INIT {
                self.flush_init(h.sb)?;
            }
            return Ok(());
        }
        if continuation && !track.pending.is_empty() {
            if track.pending_bytes + payload.len() <= MAX_PENDING_INIT {
                track.pending.push((h.clone(), payload.to_vec()));
                track.pending_bytes += payload.len();
                return Ok(());
            }
            // Too large to hold: decide on what came so far, write the rest
            // as init bytes of that part.
            self.flush_init(h.sb)?;
            return self.write_row(h.sb, h, payload, true);
        }
        self.flush_init(h.sb)?;

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
/// - Without a `tfdt` on every media segment (`WebM` blocks, continuation
///   chunks) or in sequence mode the arrival order is kept.
fn ordered_part(
    dir: &Path,
    sb: u32,
    part: u32,
    rows: &[&IndexRow],
) -> io::Result<(PathBuf, Option<f64>, usize)> {
    let raw = dir.join(format!("sb{sb}.p{part}.bin"));
    let init: Vec<&IndexRow> = rows.iter().copied().filter(|r| r.is_init).collect();
    let media: Vec<&IndexRow> = rows.iter().copied().filter(|r| !r.is_init).collect();
    let ts = rows.iter().find_map(|r| r.timescale).filter(|t| *t > 0);
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
    // Sequence mode: the browser rewrote timestampOffset itself, the recorded
    // one is stale and never used.
    let offset = |r: &IndexRow| {
        if r.seq_mode {
            0
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
    // Sequence mode: the browser placed the appends one after the other, so
    // the arrival order (duplicates already removed by the sink) is the order.
    let timed = !media.is_empty() && media.iter().all(|r| r.tfdt.is_some() && !r.seq_mode);
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
    } else if !media.is_empty() {
        let untimed = media.iter().filter(|r| r.tfdt.is_none()).count();
        eprintln!(
            "mux: sb{sb}.p{part}: {untimed} media segment(s) without tfdt; keeping arrival order"
        );
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
            (false, Some(&t)) if tracks > 1 => shift_ticks(r.tso, Some(t)),
            _ => delta(r),
        }
    };
    let rows_out: Vec<&IndexRow> = init.iter().chain(order.iter()).copied().collect();
    let sorted = write_sorted(dir, (sb, part), &mut src, &rows_out, &shift_for, shifted)?;
    Ok((sorted, start, raw_tracks))
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

fn run_mux(args: &[String]) -> i32 {
    let Some(dir) = args.first().filter(|a| !a.starts_with("--")) else {
        eprintln!("mux: capture directory required");
        return 2;
    };
    let dir = PathBuf::from(dir);
    let out = flag(args, "--out").map_or_else(|| dir.join("out.mp4"), PathBuf::from);

    let index = match fs::read_to_string(dir.join("index.jsonl")) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("mux: cannot read index.jsonl: {e}");
            return 1;
        }
    };

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
    let inputs = match mux_inputs(&dir, &rows) {
        Ok(i) => i,
        Err(code) => return code,
    };

    let mut cmd = Command::new("ffmpeg");
    run_ffmpeg(&mut cmd, &inputs, &out)
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

/// The ffmpeg inputs: per `SourceBuffer` the chosen part in timeline order.
/// `Err` carries the exit code.
fn mux_inputs(dir: &Path, rows: &[IndexRow]) -> Result<Vec<MuxInput>, i32> {
    let mut parts: HashMap<(u32, u32), (u64, bool, String)> = HashMap::new();
    for row in rows {
        let e = parts
            .entry((row.sb, row.part))
            .or_insert((0, false, row.mime.clone()));
        e.0 += row.len;
        e.1 |= row.is_init;
    }
    if parts.is_empty() {
        eprintln!("mux: index is empty");
        return Err(1);
    }

    // Adaptive players often start at a low quality and switch after the
    // first segment, which starts a new part. Pick, per SourceBuffer, the
    // decodable part (has an init segment) with the most bytes.
    let mut ids: Vec<u32> = parts.keys().map(|(sb, _)| *sb).collect();
    ids.sort_unstable();
    ids.dedup();
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
        let part_rows: Vec<&IndexRow> = rows
            .iter()
            .filter(|r| r.sb == *id && r.part == *part)
            .collect();
        let kind = part_rows
            .iter()
            .find(|r| r.is_init)
            .map_or("", |r| r.kind.as_str());
        let (path, start, traks) = match ordered_part(dir, *id, *part, &part_rows) {
            Ok(x) => x,
            Err(e) => {
                eprintln!("mux: sb{id}.p{part}: cannot reorder segments: {e}");
                return Err(1);
            }
        };
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
        let start = match probe_start(format, &path) {
            Probe::Undecodable => {
                eprintln!("mux: sb{id}.p{part} cannot be decoded; skipped");
                continue;
            }
            Probe::Start(s) => Some(s),
            Probe::Unknown => start,
        };
        inputs.push(MuxInput {
            path,
            format,
            start,
        });
    }
    if inputs.is_empty() {
        eprintln!("mux: nothing decodable to mux");
        return Err(1);
    }
    Ok(inputs)
}

/// What ffprobe says about a part.
enum Probe {
    /// First presentation time in seconds.
    Start(f64),
    /// Readable, no start time reported (or ffprobe missing).
    Unknown,
    /// ffprobe failed on it.
    Undecodable,
}

fn probe_start(format: &str, path: &Path) -> Probe {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-f", format, "-protocol_whitelist", "file"])
        .args([
            "-show_entries",
            "stream=codec_type:format=start_time",
            "-of",
            "default=noprint_wrappers=1",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .output();
    match out {
        Err(_) => Probe::Unknown,
        Ok(o) if !o.status.success() => Probe::Undecodable,
        Ok(o) => {
            let text = String::from_utf8_lossy(&o.stdout);
            // No audio or video stream (an init cut short reads "fine" but
            // has nothing to decode): ffmpeg would fail the whole mux.
            let has_stream = text
                .lines()
                .any(|l| l == "codec_type=video" || l == "codec_type=audio");
            if !has_stream {
                return Probe::Undecodable;
            }
            text.lines()
                .find_map(|l| l.strip_prefix("start_time="))
                .and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|v| v.is_finite())
                .map_or(Probe::Unknown, Probe::Start)
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

fn run_ffmpeg(cmd: &mut Command, inputs: &[MuxInput], out: &Path) -> i32 {
    cmd.arg("-hide_banner").arg("-y");
    if inputs.iter().any(|i| i.start.is_none()) && inputs.len() > 1 {
        eprintln!("mux: some inputs have no start time; their alignment is ffmpeg's guess");
    }
    for a in input_args(inputs) {
        cmd.args(a);
    }
    // Every stream of every input, not ffmpeg's "one best video, one best
    // audio" pick across inputs.
    for n in 0..inputs.len() {
        cmd.arg("-map").arg(n.to_string());
    }
    cmd.args(["-c", "copy", "-movflags", "+faststart"]).arg(out);
    cmd.stdin(Stdio::null());
    println!("mux: {} input(s) -> {}", inputs.len(), out.display());
    match cmd.status() {
        Ok(st) if st.success() => {
            println!("mux: ok");
            0
        }
        Ok(st) => {
            eprintln!("mux: ffmpeg exited with {st}");
            1
        }
        Err(e) => {
            eprintln!("mux: cannot start ffmpeg: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("streamtap-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).expect("temp dir");
        d
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
        let (path, start, _) = ordered_part(dir, 1, 0, &refs).expect("ordered");
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
        let refs: Vec<&IndexRow> = rows.iter().collect();
        let (path, _, _) = ordered_part(&dir, 1, 0, &refs).expect("ordered");
        let got = fs::read(path).expect("read");
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
        assert_eq!(
            mux_inputs(&dir, &[r]).unwrap_err(),
            1,
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
        }
    }

    fn budget(per_session: u64, total_max: u64) -> Arc<Budget> {
        Arc::new(Budget {
            per_session,
            total_max,
            total_used: AtomicU64::new(0),
        })
    }

    fn session(name: &str) -> Session {
        Session::open(
            &temp_dir(name),
            "test",
            "u",
            "t",
            budget(16 * GIB, 64 * GIB),
        )
        .expect("session")
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
        let mut s = Session::open(&out, "test", "u", "t", budget(5000, 64 * GIB)).expect("session");
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
        let shared = budget(16 * GIB, 300);
        let mut a =
            Session::open(&temp_dir("tot-a"), "a", "u", "t", Arc::clone(&shared)).expect("a");
        let mut b =
            Session::open(&temp_dir("tot-b"), "b", "u", "t", Arc::clone(&shared)).expect("b");
        write(&mut a, &seg(1, true, None, None), b"INIT");
        write(&mut b, &seg(1, true, None, None), b"INIT");
        for t in 0..10 {
            write(&mut a, &seg(1, false, Some(t), None), b"X");
            write(&mut b, &seg(1, false, Some(t), None), b"X");
        }
        assert!(shared.total_used.load(Ordering::Relaxed) <= 300);
        assert!(a.stopped || b.stopped);
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
        assert_eq!(
            mux_inputs(&dir, &[r]).unwrap_err(),
            1,
            "no complete moov: skipped"
        );
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
        let a = Session::open(&out, "same", "u", "t", budget(GIB, GIB)).expect("a");
        let b = Session::open(&out, "same", "u", "t", budget(GIB, GIB)).expect("b");
        assert_ne!(a.dir, b.dir);
        assert!(a.dir.join("session.json").is_file());
    }
}

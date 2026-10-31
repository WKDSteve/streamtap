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

/// Media segments remembered per part for dedupe: (tfdt, timestampOffset in
/// ms, duration). Bounded: beyond this the part takes no more media.
const MAX_SEEN_PER_PART: usize = 200_000;

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
    /// Init hash -> its part.
    hash_part: HashMap<u64, u32>,
    /// Parts created so far.
    parts: u32,
    /// Untimed chunks right after an init belong to it (an init split across
    /// appends), until the first timed segment.
    after_init: bool,
    /// The last timed chunk or init was a duplicate: the untimed chunks that
    /// follow it (its continuation) are duplicates too.
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
}

/// PIFF sample encryption box (`uuid` A2394F52-5A9B-4F14-A244-6C427C648DF4).
const PIFF_SAMPLE_ENCRYPTION: [u8; 16] = [
    0xa2, 0x39, 0x4f, 0x52, 0x5a, 0x9b, 0x4f, 0x14, 0xa2, 0x44, 0x6c, 0x42, 0x7c, 0x64, 0x8d, 0xf4,
];

/// True when a segment carries protected media: an init, its continuation
/// (`init_part`) or any top-level `moov` declaring encryption, a `pssh` box,
/// sample encryption (`senc`, PIFF) in a `moof`, or a `WebM` init with
/// `ContentEncryption`.
fn payload_encrypted(h: &SegmentHeader, payload: &[u8], init_part: bool) -> bool {
    if h.kind == "webm" || (init_part && !h.is_init && webm_encrypted(payload)) {
        return (h.is_init || init_part) && webm_encrypted(payload);
    }
    if (h.is_init || init_part) && looks_encrypted(payload) {
        return true;
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
                    s.summary();
                }
            }
        }
    }

    if let Some(s) = session {
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

/// What `TrackState::admit` decided for one segment.
#[derive(Debug, PartialEq, Eq)]
enum Admit {
    Skip,
    /// Write it; `as_init` marks init bytes (an init or its continuation).
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
            after_init: false,
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

    /// Decides whether a segment is written, and switches part files when an
    /// init segment starts a new one or returns to an earlier one.
    fn admit(&mut self, h: &SegmentHeader, payload: &[u8], dir: &Path) -> io::Result<Admit> {
        let timed = h.tfdt.is_some();
        let init_part = !h.is_init && !timed && (h.init_cont || self.after_init);
        if payload_encrypted(h, payload, init_part) {
            if !self.refused {
                eprintln!(
                    "{}: sb{} carries encrypted media (DRM); not capturing this track",
                    dir.display(),
                    h.sb
                );
            }
            self.refused = true;
            self.after_init = false;
            self.skipped += 1;
            return Ok(Admit::Skip);
        }
        if self.refused && !h.is_init {
            self.skipped += 1;
            return Ok(Admit::Skip);
        }

        if h.is_init {
            return self.admit_init(h, payload, dir);
        }
        if !timed {
            // Untimed: the continuation of an init or of a media segment.
            if self.skip_tail {
                self.dupes += 1;
                return Ok(Admit::Skip);
            }
            return Ok(Admit::Write { as_init: init_part });
        }
        let t = h.tfdt.unwrap_or(0);
        self.after_init = false;
        self.skip_tail = false;
        // In sequence mode the browser rewrites timestampOffset itself: the
        // recorded one says nothing, so the key is (tfdt, duration).
        let tso = if h.seq_mode { 0 } else { tso_millis(h.tso) };
        let key = (t, tso, h.dur.unwrap_or(0));
        if self.seen.contains(&key) {
            self.dupes += 1;
            self.skip_tail = true;
            return Ok(Admit::Skip);
        }
        if self.seen.len() >= MAX_SEEN_PER_PART {
            self.skipped += 1;
            return Ok(Admit::Skip);
        }
        self.seen.insert(key);
        Ok(Admit::Write { as_init: false })
    }

    fn admit_init(&mut self, h: &SegmentHeader, payload: &[u8], dir: &Path) -> io::Result<Admit> {
        let hash = fnv1a(payload);
        if self.init_hash == Some(hash) && !self.refused {
            // Same init again (player re-appends after a seek): skip it and
            // its continuation chunks.
            self.dupes += 1;
            self.skip_tail = true;
            self.after_init = false;
            return Ok(Admit::Skip);
        }
        self.refused = false;
        self.init_hash = Some(hash);
        if let Some(&earlier) = self.hash_part.get(&hash) {
            // Back to an earlier init (an ABR switch back): continue its
            // part, whose file already starts with this init.
            self.switch_part(dir, h.sb, earlier)?;
            println!("sb{} init seen before, continuing part {earlier}", h.sb);
            self.dupes += 1;
            self.skip_tail = true;
            self.after_init = false;
            return Ok(Admit::Skip);
        }
        // A new init: a new part, except for the very first init of an empty
        // track. Media that came before the first init (the hook attached
        // late) stays alone in its part.
        if !self.hash_part.is_empty() || self.offset > 0 {
            let part = self.parts;
            self.parts += 1;
            self.switch_part(dir, h.sb, part)?;
            println!("sb{} new init segment, starting part {part}", h.sb);
        }
        self.hash_part.insert(hash, self.part);
        self.after_init = true;
        self.skip_tail = false;
        Ok(Admit::Write { as_init: true })
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
        // Written atomically: a reader never sees half a file.
        let tmp = dir.join("session.json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(&meta)?)?;
        fs::rename(&tmp, dir.join("session.json"))?;
        let index = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("index.jsonl"))?;
        Ok(Self {
            dir,
            index,
            tracks: HashMap::new(),
            track_limit_logged: false,
            bytes_total: 0,
            budget,
            stopped: false,
        })
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

    fn write_segment(&mut self, h: &SegmentHeader, payload: &[u8]) -> io::Result<()> {
        let dir = self.dir.clone();
        if self.stopped || payload.is_empty() {
            return Ok(());
        }
        if !self.tracks.contains_key(&h.sb) && self.tracks.len() >= MAX_TRACKS {
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
        if let std::collections::hash_map::Entry::Vacant(v) = self.tracks.entry(h.sb) {
            v.insert(TrackState::new(Self::open_part(&dir, h.sb, 0)?));
        }
        let track = self.tracks.get_mut(&h.sb).expect("present");
        let Admit::Write { as_init } = track.admit(h, payload, &dir)? else {
            return Ok(());
        };

        // The index line is serialized first and written in one call, so an
        // interrupted sink leaves at most a missing line, not half of one.
        let line = IndexLine {
            sb: h.sb,
            ms: h.ms,
            part: track.part,
            seq: h.seq,
            offset: track.offset,
            len: payload.len(),
            is_init: as_init,
            tfdt: h.tfdt,
            timescale: h.timescale,
            dur: h.dur,
            tso: h.tso.filter(|v| v.is_finite()),
            seq_mode: h.seq_mode,
            mime: &h.mime,
            kind: &h.kind,
        };
        let mut line_buf = serde_json::to_vec(&line)?;
        line_buf.push(b'\n');
        // Payload and index line count against the budget.
        let cost = (payload.len() + line_buf.len()) as u64;
        if !self.within_budget(cost) {
            return Ok(());
        }
        let track = self.tracks.get_mut(&h.sb).expect("present");
        track.file.write_all(payload)?;
        self.index.write_all(&line_buf)?;
        track.offset += payload.len() as u64;
        track.bytes += payload.len() as u64;
        track.segments += 1;
        Ok(())
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
fn shift_tfdt(seg: &mut [u8], delta: i64) -> bool {
    let mut fields = Vec::new(); // (offset, is_64bit)
    for (kind, s, e) in boxes_in(seg, 0, seg.len()) {
        if &kind != b"moof" {
            continue;
        }
        for (kind, s, e) in boxes_in(seg, s, e) {
            if &kind != b"traf" {
                continue;
            }
            for (kind, s, e) in boxes_in(seg, s, e) {
                if &kind == b"tfdt" && e >= s + 8 {
                    let v1 = seg[s] == 1;
                    if !v1 || e >= s + 12 {
                        fields.push((s + 4, v1));
                    }
                }
            }
        }
    }
    let mut writes = Vec::new();
    for &(off, v1) in &fields {
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
) -> io::Result<(PathBuf, Option<f64>)> {
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
    let tracks = trak_count(&init_bytes).max(1);
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
    let can_shift = tracks == 1;
    if wants_shift && !can_shift {
        eprintln!(
            "mux: sb{sb}.p{part}: {tracks} tracks in one SourceBuffer; timestampOffset is not written into the segments, the timeline may jump"
        );
    }
    // Order and coverage always follow the presentation time; the tfdt is
    // only rewritten where one timescale holds for the whole segment.
    let delta = |r: &IndexRow| if can_shift { offset(r) } else { 0 };
    let pts = |r: &IndexRow| i128::from(r.tfdt.unwrap_or(0)) + i128::from(offset(r));
    let timed = !media.is_empty() && media.iter().all(|r| r.tfdt.is_some());
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
        return Ok((raw, start));
    }

    let sorted = dir.join(format!("sb{sb}.p{part}.sorted.bin"));
    let mut dst = io::BufWriter::new(File::create(&sorted)?);
    let mut unshifted = 0usize;
    for r in init.iter().chain(order.iter()) {
        read_segment(&mut src, r, &mut buf)?;
        let d = delta(r);
        if !r.is_init && d != 0 && !shift_tfdt(&mut buf, d) {
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
    Ok((sorted, start))
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
        let (path, start) = match ordered_part(dir, *id, *part, &part_rows) {
            Ok(x) => x,
            Err(e) => {
                eprintln!("mux: sb{id}.p{part}: cannot reorder segments: {e}");
                return Err(1);
            }
        };
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
            "format=start_time",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .output();
    match out {
        Err(_) => Probe::Unknown,
        Ok(o) if !o.status.success() => Probe::Undecodable,
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .map_or(Probe::Unknown, Probe::Start),
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
        let (path, start) = ordered_part(dir, 1, 0, &refs).expect("ordered");
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
        assert!(payload_encrypted(&h, &senc, false), "senc in a moof");
        assert!(!payload_encrypted(&h, &moof(0), false), "clear moof");
        let mut moov = mp4_box(*b"ftyp", b"iso6");
        moov.extend(mp4_box(*b"moov", b"..encv.."));
        assert!(
            payload_encrypted(&h, &moov, false),
            "moov in a chunk not flagged as init"
        );
        h.kind = "webm".into();
        h.is_init = true;
        assert!(payload_encrypted(
            &h,
            &[0x16, 0x54, 0x6d, 0x80, 0x50, 0x35, 0x47, 0xe2],
            false
        ));
        assert!(!payload_encrypted(&h, &[0x16, 0x54, 0xae, 0x6b], false));
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
            b"INITCCCCAAAA",
            "sequence mode: by tfdt, offset not used"
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
        let (path, _) = ordered_part(&dir, 1, 0, &refs).expect("ordered");
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
        assert_eq!(index_lines(&s.dir), 1, "only the clear first chunk");
    }

    #[test]
    fn piff_sample_encryption_is_refused() {
        let mut uuid = PIFF_SAMPLE_ENCRYPTION.to_vec();
        uuid.extend_from_slice(b"rest");
        let piff = mp4_box(*b"moof", &mp4_box(*b"traf", &mp4_box(*b"uuid", &uuid)));
        assert!(payload_encrypted(
            &seg(1, false, Some(0), None),
            &piff,
            false
        ));
    }

    #[test]
    fn session_budget_counts_index_lines_and_stops_for_good() {
        let out = temp_dir("cap");
        let mut s = Session::open(&out, "test", "u", "t", budget(400, 64 * GIB)).expect("session");
        write(&mut s, &seg(1, true, None, None), b"INIT");
        write(&mut s, &seg(1, false, Some(0), None), b"");
        assert_eq!(index_lines(&s.dir), 1, "an empty payload is not written");
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

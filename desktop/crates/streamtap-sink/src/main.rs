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
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const MAX_HEADER: usize = 64 * 1024;
const MAX_MESSAGE: usize = 64 * 1024 * 1024;
/// `SourceBuffers` per session; a page can forge records with arbitrary ids,
/// and each track holds an open file.
const MAX_TRACKS: usize = 32;
/// Malformed messages tolerated per connection after `hello` before closing.
const MAX_BAD_MESSAGES: u32 = 100;
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
                std::thread::spawn(move || {
                    if let Err(e) = handle_connection(s, &token, &origin, &out) {
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
    #[serde(default)]
    kind: String,
    #[serde(default)]
    tfdt: Option<u64>,
    #[serde(default)]
    timescale: Option<u32>,
    /// `SourceBuffer.timestampOffset` in seconds at the time of the append.
    /// Multi-period DASH and HLS discontinuities restart `tfdt` and shift the
    /// timeline with it, so dedupe and ordering must include it.
    #[serde(default)]
    tso: Option<f64>,
}

#[derive(Debug, Serialize)]
struct IndexLine<'a> {
    sb: u32,
    ms: Option<u32>,
    part: u32,
    seq: u64,
    offset: u64,
    len: usize,
    is_init: bool,
    tfdt: Option<u64>,
    timescale: Option<u32>,
    tso: Option<f64>,
    mime: &'a str,
    kind: &'a str,
}

struct TrackState {
    part: u32,
    file: File,
    offset: u64,
    init_hash: Option<u64>,
    /// (tfdt, timestampOffset in ms) of every media segment of this part.
    seen: HashSet<(u64, i64)>,
    /// The current init segment is encrypted (EME/DRM): nothing is written
    /// until a clear init segment arrives.
    refused: bool,
    bytes: u64,
    segments: u64,
    dupes: u64,
    skipped: u64,
}

struct Session {
    dir: PathBuf,
    index: File,
    tracks: HashMap<u32, TrackState>,
    track_limit_logged: bool,
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

// The handshake closure returns tungstenite's fixed (large) error type.
#[allow(clippy::result_large_err)]
fn handle_connection(stream: TcpStream, token: &[u8], origin: &str, out: &Path) -> io::Result<()> {
    let peer = stream.peer_addr()?;
    let mut ws = tungstenite::accept_hdr(stream, |req: &_, resp| check_origin(origin, req, resp))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{peer}: {e}")))?;
    let mut session: Option<Session> = None;
    let mut bad_messages = 0u32;

    loop {
        let msg = match ws.read() {
            Ok(m) => m,
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => break,
            Err(e) => {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    e.to_string(),
                ))
            }
        };
        let data = match msg {
            tungstenite::Message::Binary(b) => b,
            tungstenite::Message::Close(_) => {
                // tungstenite queued the close reply; send it so the client
                // sees a clean close instead of a dropped connection.
                let _ = ws.flush();
                break;
            }
            tungstenite::Message::Ping(p) => {
                let _ = ws.send(tungstenite::Message::Pong(p));
                continue;
            }
            _ => continue,
        };
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
                let s = Session::open(out, &id, &url, &title)?;
                println!("{peer}: session {} ({url})", s.dir.display());
                session = Some(s);
            }
            (Header::Hello { .. }, Some(_)) => {
                eprintln!("{peer}: second hello, ignoring");
            }
            (_, None) => {
                eprintln!("{peer}: message before hello, closing");
                break;
            }
            (Header::Segment(h), Some(s)) => s.write_segment(&h, payload)?,
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

impl Session {
    fn open(out: &Path, session_id: &str, url: &str, title: &str) -> io::Result<Self> {
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
        })
    }

    fn open_part(dir: &Path, sb: u32, part: u32) -> io::Result<File> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(format!("sb{sb}.p{part}.bin")))
    }

    fn write_segment(&mut self, h: &SegmentHeader, payload: &[u8]) -> io::Result<()> {
        let dir = self.dir.clone();
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
        let track = if let Some(t) = self.tracks.get_mut(&h.sb) {
            t
        } else {
            let file = Self::open_part(&dir, h.sb, 0)?;
            self.tracks.insert(
                h.sb,
                TrackState {
                    part: 0,
                    file,
                    offset: 0,
                    init_hash: None,
                    seen: HashSet::new(),
                    refused: false,
                    bytes: 0,
                    segments: 0,
                    dupes: 0,
                    skipped: 0,
                },
            );
            self.tracks.get_mut(&h.sb).expect("just inserted")
        };

        if h.is_init && looks_encrypted(payload) {
            if !track.refused {
                eprintln!(
                    "{}: sb{} init segment is encrypted (DRM); not capturing this track",
                    dir.display(),
                    h.sb
                );
            }
            track.refused = true;
            track.skipped += 1;
            return Ok(());
        }
        if track.refused && !h.is_init {
            track.skipped += 1;
            return Ok(());
        }

        if h.is_init {
            let hash = fnv1a(payload);
            match track.init_hash {
                Some(prev) if prev == hash && !track.refused => {
                    // Same init again (player re-appends after a seek): skip.
                    track.dupes += 1;
                    return Ok(());
                }
                Some(_) => {
                    // Different codec configuration (or clear again after an
                    // encrypted stretch): start a new part file.
                    track.part += 1;
                    track.file = Self::open_part(&dir, h.sb, track.part)?;
                    track.offset = 0;
                    track.seen.clear();
                    println!("sb{} new init segment, starting part {}", h.sb, track.part);
                }
                None => {}
            }
            track.refused = false;
            track.init_hash = Some(hash);
        } else if let Some(t) = h.tfdt {
            if !track.seen.insert((t, tso_millis(h.tso))) {
                track.dupes += 1;
                return Ok(());
            }
        }

        // The index line is serialized first and written in one call, so an
        // interrupted sink leaves at most a missing line, not half of one.
        let line = IndexLine {
            sb: h.sb,
            ms: h.ms,
            part: track.part,
            seq: h.seq,
            offset: track.offset,
            len: payload.len(),
            is_init: h.is_init,
            tfdt: h.tfdt,
            timescale: h.timescale,
            tso: h.tso.filter(|v| v.is_finite()),
            mime: &h.mime,
            kind: &h.kind,
        };
        let mut line_buf = serde_json::to_vec(&line)?;
        line_buf.push(b'\n');
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
                t.part + 1,
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
    tso: Option<f64>,
    mime: String,
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

/// Returns a file for `sbN.pM` with the init segment first and the media
/// segments in presentation order (`tfdt` plus `timestampOffset`). Players
/// fetch out of order after a backward seek, and the sink writes in arrival
/// order. If the raw part already is in that order it is returned unchanged;
/// otherwise `sbN.pM.sorted.bin` is written. Segments shifted by a
/// `timestampOffset` get their `tfdt` rewritten so the timeline stays
/// monotonic. Without a `tfdt` on every segment (`WebM` blocks, continuation
/// chunks) the media order is the arrival order.
fn ordered_part(dir: &Path, sb: u32, part: u32, rows: &[&IndexRow]) -> io::Result<PathBuf> {
    let raw = dir.join(format!("sb{sb}.p{part}.bin"));
    let init: Vec<&IndexRow> = rows.iter().copied().filter(|r| r.is_init).collect();
    let mut media: Vec<&IndexRow> = rows.iter().copied().filter(|r| !r.is_init).collect();
    let ts = rows.iter().find_map(|r| r.timescale).filter(|t| *t > 0);
    let shift = |r: &IndexRow| -> i64 {
        match (ts, r.tso) {
            (Some(ts), Some(_)) => tso_millis(r.tso).saturating_mul(i64::from(ts)) / 1000,
            _ => 0,
        }
    };
    let pts = |r: &IndexRow| i128::from(r.tfdt.unwrap_or(0)) + i128::from(shift(r));
    let shifted = media.iter().any(|r| shift(r) != 0);
    let all_timed = media.iter().all(|r| r.tfdt.is_some());
    let init_first = rows.first().is_some_and(|r| r.is_init);

    let mut in_order = init_first && !shifted;
    if all_timed {
        in_order = in_order && media.windows(2).all(|w| pts(w[0]) < pts(w[1]));
        media.sort_by_key(|r| pts(r));
        // Report gaps (missed ranges after a forward seek) on the timeline.
        if let Some(ts) = ts {
            let steps: Vec<u64> = media
                .windows(2)
                .filter_map(|w| u64::try_from(pts(w[1]) - pts(w[0])).ok())
                .collect();
            let mut typical = steps.clone();
            typical.sort_unstable();
            if let Some(&median) = typical.get(typical.len() / 2) {
                let big: Vec<u64> = steps
                    .iter()
                    .copied()
                    .filter(|s| *s > median * 3 / 2)
                    .collect();
                if !big.is_empty() {
                    let missing: u64 = big.iter().map(|s| s - median).sum();
                    eprintln!(
                        "mux: sb{sb}.p{part}: {} gap(s), about {} s not captured (seeked past them)",
                        big.len(),
                        missing / u64::from(ts)
                    );
                }
            }
        }
    } else if !media.is_empty() {
        let untimed = media.iter().filter(|r| r.tfdt.is_none()).count();
        eprintln!(
            "mux: sb{sb}.p{part}: {untimed} media segment(s) without tfdt; keeping arrival order"
        );
    }
    if in_order {
        return Ok(raw);
    }

    let sorted = dir.join(format!("sb{sb}.p{part}.sorted.bin"));
    let mut src = File::open(&raw)?;
    let mut dst = io::BufWriter::new(File::create(&sorted)?);
    let mut buf = Vec::new();
    let mut unshifted = 0usize;
    for r in init.iter().take(1).chain(media.iter()) {
        let len = usize::try_from(r.len)
            .ok()
            .filter(|l| *l <= MAX_MESSAGE)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "segment too large"))?;
        buf.resize(len, 0);
        src.seek(io::SeekFrom::Start(r.offset))?;
        src.read_exact(&mut buf)?;
        let delta = shift(r);
        if !r.is_init && delta != 0 && !shift_tfdt(&mut buf, delta) {
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

    // Per (SourceBuffer, part): bytes written, whether it starts with an init
    // segment, and the MIME type.
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

/// The ffmpeg inputs: per `SourceBuffer` the chosen part in timeline order.
/// `Err` carries the exit code.
fn mux_inputs(dir: &Path, rows: &[IndexRow]) -> Result<Vec<PathBuf>, i32> {
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
                "mux: sb{id} has {total} parts (codec/quality changes); using part {part} ({bytes} bytes, {mime})"
            );
        }
        let part_rows: Vec<&IndexRow> = rows
            .iter()
            .filter(|r| r.sb == *id && r.part == *part)
            .collect();
        match ordered_part(dir, *id, *part, &part_rows) {
            Ok(p) => inputs.push(p),
            Err(e) => {
                eprintln!("mux: sb{id}.p{part}: cannot reorder segments: {e}");
                return Err(1);
            }
        }
    }
    if inputs.is_empty() {
        eprintln!("mux: nothing decodable to mux");
        return Err(1);
    }
    Ok(inputs)
}

fn run_ffmpeg(cmd: &mut Command, inputs: &[PathBuf], out: &Path) -> i32 {
    cmd.arg("-hide_banner").arg("-y");
    for i in inputs {
        cmd.arg("-i").arg(i);
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

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("streamtap-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).expect("temp dir");
        d
    }

    /// A minimal media segment: moof > traf > tfdt (version 0) with `tfdt`.
    fn moof(tfdt: u32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&32u32.to_be_bytes());
        v.extend_from_slice(b"moof");
        v.extend_from_slice(&24u32.to_be_bytes());
        v.extend_from_slice(b"traf");
        v.extend_from_slice(&16u32.to_be_bytes());
        v.extend_from_slice(b"tfdt");
        v.extend_from_slice(&[0, 0, 0, 0]);
        v.extend_from_slice(&tfdt.to_be_bytes());
        v
    }

    fn tfdt_of(seg: &[u8]) -> u32 {
        u32::from_be_bytes([seg[28], seg[29], seg[30], seg[31]])
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
            tso,
            mime: "video/mp4".into(),
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
    fn encrypted_init_is_recognised() {
        assert!(looks_encrypted(b"\0\0\0\x10moov....encv...."));
        assert!(looks_encrypted(b"xxpsshxx"));
        assert!(!looks_encrypted(b"\0\0\0\x10moov....avc1...."));
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
        // size 3 (< header), size beyond the end, 64-bit size cut short.
        for buf in [
            b"\0\0\0\x03moofxxxx".to_vec(),
            b"\0\0\x10\0moofxxxx".to_vec(),
            b"\0\0\0\x01moof\0\0".to_vec(),
        ] {
            assert_eq!(boxes_in(&buf, 0, buf.len()), Vec::new());
        }
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
        let refs: Vec<&IndexRow> = rows.iter().collect();
        let out = ordered_part(&dir, 1, 0, &refs).expect("ordered");
        assert!(out.ends_with("sb1.p0.sorted.bin"));
        let got = fs::read(out).expect("read");
        assert_eq!(&got[..4], b"INIT");
        assert_eq!(tfdt_of(&got[4..]), 1000);
        assert_eq!(tfdt_of(&got[36..]), 2000);
    }

    #[test]
    fn ordered_part_keeps_arrival_order_without_tfdt() {
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
        let refs: Vec<&IndexRow> = rows.iter().collect();
        let out = ordered_part(&dir, 1, 0, &refs).expect("ordered");
        assert!(
            out.ends_with("sb1.p0.bin"),
            "already in arrival order: raw file"
        );
        assert_eq!(fs::read(out).expect("read"), b"INITAAAABBBBCCCC");
    }

    #[test]
    fn ordered_part_moves_init_to_the_front() {
        let dir = temp_dir("initlate");
        let rows = write_part(
            &dir,
            &[(b"MMMM", false, None, None), (b"INIT", true, None, None)],
        );
        let refs: Vec<&IndexRow> = rows.iter().collect();
        let out = ordered_part(&dir, 1, 0, &refs).expect("ordered");
        assert_eq!(fs::read(out).expect("read"), b"INITMMMM");
    }

    #[test]
    fn ordered_part_applies_timestamp_offset() {
        // Second period restarts tfdt at 0 with timestampOffset 10 s.
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
        let refs: Vec<&IndexRow> = rows.iter().collect();
        let got = fs::read(ordered_part(&dir, 1, 0, &refs).expect("ordered")).expect("read");
        assert_eq!(tfdt_of(&got[4..]), 5000);
        assert_eq!(
            tfdt_of(&got[36..]),
            10_000,
            "tfdt shifted by 10 s at timescale 1000"
        );
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
        let json = br#"{"type":"segment","sb":"x","seq":1}"#;
        let mut forged = u32::try_from(json.len())
            .expect("small")
            .to_le_bytes()
            .to_vec();
        forged.extend_from_slice(json);
        assert!(
            split_message(&forged).is_err(),
            "a string sb id is rejected"
        );
    }

    fn seg(sb: u32, is_init: bool, tfdt: Option<u64>, tso: Option<f64>) -> SegmentHeader {
        SegmentHeader {
            sb,
            seq: 1,
            ms: Some(1),
            mime: "video/mp4".into(),
            is_init,
            kind: "mp4".into(),
            tfdt,
            timescale: Some(1000),
            tso,
        }
    }

    fn index_lines(dir: &Path) -> usize {
        fs::read_to_string(dir.join("index.jsonl"))
            .expect("index")
            .lines()
            .count()
    }

    #[test]
    fn dedupe_includes_timestamp_offset() {
        let out = temp_dir("dedupe");
        let mut s = Session::open(&out, "test", "u", "t").expect("session");
        s.write_segment(&seg(1, true, None, None), b"INIT")
            .expect("init");
        s.write_segment(&seg(1, false, Some(0), None), b"P1")
            .expect("p1");
        s.write_segment(&seg(1, false, Some(0), None), b"P1")
            .expect("dupe");
        s.write_segment(&seg(1, false, Some(0), Some(10.0)), b"P2")
            .expect("p2");
        assert_eq!(
            index_lines(&s.dir),
            3,
            "init + period 1 + period 2, one dupe dropped"
        );
    }

    #[test]
    fn encrypted_track_is_not_written() {
        let out = temp_dir("drm");
        let mut s = Session::open(&out, "test", "u", "t").expect("session");
        s.write_segment(&seg(1, true, None, None), b"....encv....")
            .expect("init");
        s.write_segment(&seg(1, false, Some(0), None), b"CIPHER")
            .expect("media");
        assert_eq!(index_lines(&s.dir), 0);
        assert_eq!(fs::read(s.dir.join("sb1.p0.bin")).expect("part").len(), 0);
        // A clear init afterwards is captured again.
        s.write_segment(&seg(1, true, None, None), b"CLEARINIT")
            .expect("clear");
        s.write_segment(&seg(1, false, Some(0), None), b"MEDIA")
            .expect("media");
        assert_eq!(index_lines(&s.dir), 2);
    }

    #[test]
    fn track_count_is_bounded() {
        let out = temp_dir("tracks");
        let mut s = Session::open(&out, "test", "u", "t").expect("session");
        let limit = u32::try_from(MAX_TRACKS).expect("small constant");
        for sb in 0..(limit + 10) {
            s.write_segment(&seg(sb, true, None, None), b"I")
                .expect("init");
        }
        assert_eq!(s.tracks.len(), MAX_TRACKS);
    }

    #[test]
    fn sessions_never_share_a_directory() {
        let out = temp_dir("dirs");
        let a = Session::open(&out, "same", "u", "t").expect("a");
        let b = Session::open(&out, "same", "u", "t").expect("b");
        assert_ne!(a.dir, b.dir);
        assert!(a.dir.join("session.json").is_file());
    }
}

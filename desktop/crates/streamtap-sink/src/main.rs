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
use std::io::{self, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const MAX_HEADER: usize = 64 * 1024;
const MAX_MESSAGE: usize = 64 * 1024 * 1024;
const DEFAULT_PORT: u16 = 17_650;

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
                "usage:\n  streamtap-sink serve --token <token> [--port {DEFAULT_PORT}] [--out captures]\n  streamtap-sink mux <capture-dir> [--out <file.mp4>]"
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
        "streamtap-sink listening on ws://127.0.0.1:{port}  out={}",
        out.display()
    );

    let token = token.as_bytes().to_vec();
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let token = token.clone();
                let out = out.clone();
                std::thread::spawn(move || {
                    if let Err(e) = handle_connection(s, &token, &out) {
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
    #[serde(default)]
    mime: String,
    #[serde(default, rename = "isInit")]
    is_init: bool,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    tfdt: Option<u64>,
    #[serde(default)]
    timescale: Option<u64>,
}

#[derive(Debug, Serialize)]
struct IndexLine<'a> {
    sb: u32,
    part: u32,
    seq: u64,
    offset: u64,
    len: usize,
    is_init: bool,
    tfdt: Option<u64>,
    timescale: Option<u64>,
    mime: &'a str,
    kind: &'a str,
}

struct TrackState {
    part: u32,
    file: File,
    offset: u64,
    init_hash: Option<u64>,
    seen_tfdt: HashSet<u64>,
    bytes: u64,
    segments: u64,
    dupes: u64,
}

struct Session {
    dir: PathBuf,
    index: File,
    tracks: HashMap<u32, TrackState>,
}

fn handle_connection(stream: TcpStream, token: &[u8], out: &Path) -> io::Result<()> {
    let peer = stream.peer_addr()?;
    let mut ws = tungstenite::accept(stream)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let mut session: Option<Session> = None;

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
            tungstenite::Message::Close(_) => break,
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
            Err(e) => {
                eprintln!("{peer}: bad message: {e}");
                break;
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
        let name = format!("{secs}_{}", if short.is_empty() { "anon" } else { &short });
        let dir = out.join(name);
        fs::create_dir_all(&dir)?;
        let meta = serde_json::json!({ "url": url, "title": title, "session": session_id, "started": secs });
        fs::write(dir.join("session.json"), serde_json::to_vec_pretty(&meta)?)?;
        let index = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("index.jsonl"))?;
        Ok(Self {
            dir,
            index,
            tracks: HashMap::new(),
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
                    seen_tfdt: HashSet::new(),
                    bytes: 0,
                    segments: 0,
                    dupes: 0,
                },
            );
            self.tracks.get_mut(&h.sb).expect("just inserted")
        };

        if h.is_init {
            let hash = fnv1a(payload);
            match track.init_hash {
                Some(prev) if prev == hash => {
                    // Same init again (player re-appends after a seek): skip.
                    track.dupes += 1;
                    return Ok(());
                }
                Some(_) => {
                    // Different codec configuration: start a new part file.
                    track.part += 1;
                    track.file = Self::open_part(&dir, h.sb, track.part)?;
                    track.offset = 0;
                    track.seen_tfdt.clear();
                    println!("sb{} new init segment, starting part {}", h.sb, track.part);
                }
                None => {}
            }
            track.init_hash = Some(hash);
        } else if let Some(t) = h.tfdt {
            if !track.seen_tfdt.insert(t) {
                track.dupes += 1;
                return Ok(());
            }
        }

        track.file.write_all(payload)?;
        let line = IndexLine {
            sb: h.sb,
            part: track.part,
            seq: h.seq,
            offset: track.offset,
            len: payload.len(),
            is_init: h.is_init,
            tfdt: h.tfdt,
            timescale: h.timescale,
            mime: &h.mime,
            kind: &h.kind,
        };
        serde_json::to_writer(&mut self.index, &line)?;
        self.index.write_all(b"\n")?;
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
                "{}: sb{id} parts={} segments={} dupes={} bytes={}",
                self.dir.display(),
                t.part + 1,
                t.segments,
                t.dupes,
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
    part: u32,
    is_init: bool,
    mime: String,
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

    // Per SourceBuffer: which parts exist, and whether part 0 has an init segment.
    let mut parts: HashMap<u32, (u32, bool, String)> = HashMap::new();
    for line in index.lines().filter(|l| !l.trim().is_empty()) {
        let row: IndexRow = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("mux: bad index line: {e}");
                return 1;
            }
        };
        let e = parts.entry(row.sb).or_insert((0, false, row.mime.clone()));
        e.0 = e.0.max(row.part);
        if row.part == 0 && row.is_init {
            e.1 = true;
        }
    }
    if parts.is_empty() {
        eprintln!("mux: index is empty");
        return 1;
    }

    let mut ids: Vec<_> = parts.keys().copied().collect();
    ids.sort_unstable();
    let mut inputs = Vec::new();
    for id in &ids {
        let (max_part, has_init, mime) = &parts[id];
        if !has_init {
            eprintln!("mux: sb{id} ({mime}) has no init segment in part 0; the file will not decode. Capture with the hook active from page load.");
        }
        if *max_part > 0 {
            eprintln!("mux: sb{id} has {} parts (codec/quality changes); only part 0 is muxed in this spike", max_part + 1);
        }
        inputs.push(dir.join(format!("sb{id}.p0.bin")));
    }

    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-hide_banner").arg("-y");
    for i in &inputs {
        cmd.arg("-i").arg(i);
    }
    cmd.args(["-c", "copy", "-movflags", "+faststart"])
        .arg(&out);
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

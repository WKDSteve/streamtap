//! Security tests of the sink's front door, as raw TCP bytes (TST-01, SRV-23, SRV-07).
//!
//! The real binary is started on port 0 (the port it prints is the start signal, TST-14) with its
//! capture folder under the system temp folder (TST-03), and a hand-made WebSocket client talks to
//! it: no library on this side, so a frame the library would refuse to build can be sent. Nothing
//! here touches the user's ports, `AppData` or `captures/`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const TOKEN: &str = "tok-test-123";
const ORIGIN: &str = "http://test.invalid";

/// A running sink and its capture folder; both are removed on drop.
struct Sink {
    child: Child,
    port: u16,
    out: PathBuf,
}

impl Sink {
    fn start(name: &str, extra: &[&str]) -> Self {
        let out =
            std::env::temp_dir().join(format!("streamtap-sink-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        let mut child = Command::new(env!("CARGO_BIN_EXE_streamtap-sink"))
            .args([
                "serve",
                "--token",
                TOKEN,
                "--port",
                "0",
                "--allow-origin",
                ORIGIN,
                "--out",
            ])
            .arg(&out)
            .args(extra)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start the sink");
        let mut lines = BufReader::new(child.stdout.take().expect("stdout"));
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let _ = lines.read_line(&mut line);
            let _ = tx.send(line);
            // Keep reading so the sink never blocks on a full pipe.
            let _ = std::io::copy(&mut lines, &mut std::io::sink());
        });
        let line = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the sink prints its start line");
        let port = line
            .split("ws://127.0.0.1:")
            .nth(1)
            .and_then(|r| r.split_whitespace().next())
            .and_then(|p| p.parse().ok())
            .unwrap_or_else(|| panic!("no port in the start line {line:?}"));
        Self { child, port, out }
    }

    fn connect(&self) -> Client {
        let s = TcpStream::connect(("127.0.0.1", self.port)).expect("connect");
        Client { s }
    }

    /// Files and folders the sink wrote into its capture folder.
    fn written(&self) -> Vec<String> {
        std::fs::read_dir(&self.out)
            .map(|d| {
                d.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.out);
    }
}

struct Client {
    s: TcpStream,
}

impl Client {
    fn handshake_request(origin: &str) -> String {
        format!(
            "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\
             Origin: {origin}\r\n\r\n"
        )
    }

    /// The WebSocket upgrade; true on `101 Switching Protocols`.
    fn handshake(&mut self) -> bool {
        self.s
            .write_all(Self::handshake_request(ORIGIN).as_bytes())
            .expect("send the upgrade");
        self.s
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut got = Vec::new();
        let mut b = [0u8; 1];
        while !got.ends_with(b"\r\n\r\n") {
            match self.s.read(&mut b) {
                Ok(1) => got.push(b[0]),
                _ => return false,
            }
        }
        got.starts_with(b"HTTP/1.1 101")
    }

    /// One client frame (masked, as the protocol demands; the mask is all zero, so the payload
    /// stays readable in a dump).
    fn frame(&mut self, opcode: u8, payload: &[u8]) {
        let mut f = vec![0x80 | opcode];
        match payload.len() {
            n if n < 126 => f.push(0x80 | u8::try_from(n).unwrap()),
            n if n <= 0xffff => {
                f.push(0x80 | 0x7e);
                f.extend_from_slice(&u16::try_from(n).unwrap().to_be_bytes());
            }
            n => {
                f.push(0x80 | 0x7f);
                f.extend_from_slice(&u64::try_from(n).unwrap().to_be_bytes());
            }
        }
        f.extend_from_slice(&[0; 4]);
        f.extend_from_slice(payload);
        // A closed peer fails the write; that is a legitimate answer for these tests.
        let _ = self.s.write_all(&f);
    }

    fn binary(&mut self, header_len: u32, header: &[u8], payload: &[u8]) {
        let mut m = header_len.to_le_bytes().to_vec();
        m.extend_from_slice(header);
        m.extend_from_slice(payload);
        self.frame(0x2, &m);
    }

    /// A well-formed message: the true header length.
    fn message(&mut self, header: &str, payload: &[u8]) {
        self.binary(
            u32::try_from(header.len()).unwrap(),
            header.as_bytes(),
            payload,
        );
    }

    fn hello(&mut self, token: &str) {
        self.message(
            &format!(r#"{{"type":"hello","token":"{token}","session":"s1","url":"http://x.invalid/","title":"t"}}"#),
            b"",
        );
    }

    /// True when the sink closed the connection within `within`; false when it is still open.
    fn closed_within(&mut self, within: Duration) -> bool {
        let end = Instant::now() + within;
        let mut buf = [0u8; 4096];
        loop {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            self.s.set_read_timeout(Some(left)).unwrap();
            match self.s.read(&mut buf) {
                Ok(0) => return true,
                Ok(_) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return false
                }
                Err(_) => return true,
            }
        }
    }
}

const SOON: Duration = Duration::from_secs(5);

fn no_session(sink: &Sink) {
    assert_eq!(
        sink.written(),
        Vec::<String>::new(),
        "nothing may be written"
    );
}

#[test]
fn binds_loopback_only() {
    let mut sink = Sink::start("loopback", &[]);
    // The start line names 127.0.0.1 (the parse in `start` needs it) and a connect on it works.
    assert!(sink.connect().handshake());
    assert!(sink.alive());
}

#[test]
fn silent_connection_is_closed_after_the_hello_timeout() {
    let sink = Sink::start("silent", &["--hello-timeout-secs", "1"]);
    let mut c = sink.connect();
    assert!(
        c.closed_within(SOON),
        "a connection that never speaks must be closed"
    );
    no_session(&sink);
}

#[test]
fn a_trickling_handshake_does_not_restart_the_deadline() {
    let sink = Sink::start("trickle", &["--hello-timeout-secs", "1"]);
    let mut c = sink.connect();
    let start = Instant::now();
    let request = Client::handshake_request(ORIGIN);
    let mut bytes = request.bytes();
    // One byte every 300 ms would keep a per-read timeout of 1 s alive for ever.
    let closed = loop {
        match bytes.next() {
            Some(b) => {
                if c.s.write_all(&[b]).is_err() {
                    break true;
                }
            }
            None => break false,
        }
        if c.closed_within(Duration::from_millis(300)) {
            break true;
        }
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "still open after 8 s"
        );
    };
    assert!(closed);
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn handshake_without_hello_is_closed() {
    let sink = Sink::start("nohello", &["--hello-timeout-secs", "1"]);
    let mut c = sink.connect();
    assert!(c.handshake());
    assert!(c.closed_within(SOON));
    no_session(&sink);
}

#[test]
fn wrong_token_closes_and_writes_nothing() {
    let sink = Sink::start("wrongtoken", &[]);
    let mut c = sink.connect();
    assert!(c.handshake());
    c.hello("not-the-token");
    assert!(c.closed_within(SOON));
    no_session(&sink);
}

#[test]
fn a_hello_after_the_deadline_is_not_taken() {
    let sink = Sink::start("late", &["--hello-timeout-secs", "1"]);
    let mut c = sink.connect();
    assert!(c.handshake());
    std::thread::sleep(Duration::from_millis(1600));
    c.hello(TOKEN);
    assert!(c.closed_within(SOON));
    no_session(&sink);
}

#[test]
fn a_header_over_64_kib_closes() {
    let sink = Sink::start("bighdr", &[]);
    let mut c = sink.connect();
    assert!(c.handshake());
    c.binary(64 * 1024 + 1, &vec![b' '; 64 * 1024 + 1], b"");
    assert!(c.closed_within(SOON));
    no_session(&sink);
}

#[test]
fn a_message_over_64_mib_closes() {
    let sink = Sink::start("bigmsg", &[]);
    let mut c = sink.connect();
    assert!(c.handshake());
    // Only the frame header claims 65 MiB; the sink must refuse on the header, not wait for the bytes.
    let mut f = vec![0x82, 0x80 | 0x7f];
    f.extend_from_slice(&(65u64 * 1024 * 1024).to_be_bytes());
    f.extend_from_slice(&[0; 4]);
    c.s.write_all(&f).unwrap();
    assert!(c.closed_within(SOON));
    no_session(&sink);
}

#[test]
fn a_header_length_beyond_the_message_closes() {
    let sink = Sink::start("hlenbeyond", &[]);
    let mut c = sink.connect();
    assert!(c.handshake());
    c.binary(100, b"{}", b"");
    assert!(c.closed_within(SOON));
    no_session(&sink);
}

#[test]
fn a_broken_json_header_closes() {
    let sink = Sink::start("badjson", &[]);
    let mut c = sink.connect();
    assert!(c.handshake());
    c.message(r#"{"type":"hello","token":"#, b"");
    assert!(c.closed_within(SOON));
    no_session(&sink);
}

#[test]
fn a_text_frame_is_no_hello() {
    let sink = Sink::start("textframe", &["--hello-timeout-secs", "1"]);
    let mut c = sink.connect();
    assert!(c.handshake());
    let hello = format!(r#"{{"type":"hello","token":"{TOKEN}","session":"s1"}}"#);
    c.frame(0x1, hello.as_bytes());
    // Ignored, not taken for a hello: the deadline ends it and nothing is written.
    assert!(c.closed_within(SOON));
    no_session(&sink);
}

#[test]
fn the_connection_after_the_limit_is_refused() {
    let sink = Sink::start("limit", &["--max-connections", "2"]);
    let mut a = sink.connect();
    let mut b = sink.connect();
    assert!(a.handshake() && b.handshake());
    let mut third = sink.connect();
    assert!(
        third.closed_within(Duration::from_secs(3)),
        "the 3rd connection must be refused"
    );
    // The two in place are unharmed, and a place frees when one goes.
    assert!(!a.closed_within(Duration::from_millis(300)));
    drop(b);
    std::thread::sleep(Duration::from_millis(500));
    let mut again = sink.connect();
    assert!(again.handshake(), "a freed place can be taken");
}

#[test]
fn a_quiet_session_after_hello_is_kept_until_the_idle_timeout() {
    let sink = Sink::start(
        "idle",
        &["--hello-timeout-secs", "1", "--idle-timeout-secs", "4"],
    );
    let mut c = sink.connect();
    assert!(c.handshake());
    c.hello(TOKEN);
    // Well past the hello deadline: that clock no longer runs.
    assert!(
        !c.closed_within(Duration::from_millis(2500)),
        "closed by the hello deadline"
    );
    assert!(
        c.closed_within(Duration::from_secs(8)),
        "the idle timeout must end it"
    );
}

#[test]
fn a_stream_of_pings_does_not_keep_an_idle_session_open() {
    let sink = Sink::start(
        "pings",
        &["--hello-timeout-secs", "5", "--idle-timeout-secs", "2"],
    );
    let mut c = sink.connect();
    assert!(c.handshake());
    c.hello(TOKEN);
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(6) {
        if c.closed_within(Duration::from_millis(200)) {
            let took = start.elapsed();
            assert!(took >= Duration::from_secs(1), "closed too early: {took:?}");
            return;
        }
        c.frame(0x9, b"still here");
    }
    panic!("the pings kept the session open past the idle timeout");
}

#[test]
fn silent_connections_do_not_lock_out_a_client() {
    // Well above --max-connections: a local process that keeps connecting and says nothing.
    let sink = Sink::start("lockout", &["--max-connections", "8"]);
    let mut settled = sink.connect();
    assert!(settled.handshake());
    settled.hello(TOKEN);
    std::thread::sleep(Duration::from_millis(300));
    let silent: Vec<Client> = (0..20).map(|_| sink.connect()).collect();
    std::thread::sleep(Duration::from_millis(300));

    let mut late = sink.connect();
    assert!(late.handshake(), "a new client gets past the silent ones");
    late.hello(TOKEN);
    assert!(
        !late.closed_within(Duration::from_millis(300)),
        "and stays connected"
    );
    assert!(
        !settled.closed_within(Duration::from_millis(100)),
        "a session after hello is never pushed out"
    );
    drop(silent);
}

#[test]
fn hostile_boxes_do_not_stop_the_sink() {
    let mut sink = Sink::start("hostile", &[]);
    let mut c = sink.connect();
    assert!(c.handshake());
    c.hello(TOKEN);
    let init = r#"{"type":"segment","sb":0,"seq":0,"isInit":true,"kind":"mp4"}"#;
    let media = |seq: u32| {
        format!(r#"{{"type":"segment","sb":0,"seq":{seq},"kind":"mp4","tfdt":0,"timescale":1000}}"#)
    };
    let be = |n: u32| n.to_be_bytes().to_vec();
    let boxed = |size: u32, kind: &[u8; 4], rest: &[u8]| {
        let mut v = be(size);
        v.extend_from_slice(kind);
        v.extend_from_slice(rest);
        v
    };
    // A truncated ftyp, a box of size 0, a box that claims 4 GiB, a moof without tfdt, a moof that
    // ends inside its own header, an empty payload.
    c.message(init, &boxed(100, b"ftyp", b"isom"));
    c.message(&media(1), &boxed(0, b"moof", b""));
    c.message(&media(2), &boxed(u32::MAX, b"mdat", b"x"));
    c.message(&media(3), &boxed(24, b"moof", &boxed(16, b"mfhd", &[0; 8])));
    c.message(&media(4), &boxed(12, b"moof", &[0xff; 4]));
    c.message(&media(5), b"");
    std::thread::sleep(Duration::from_millis(500));
    assert!(sink.alive(), "the sink must outlive hostile input");
    let mut next = sink.connect();
    assert!(next.handshake(), "and take the next client");
}

#[test]
fn files_stay_inside_the_capture_folder() {
    // The session id is the page's: a path in it must not leave the folder.
    let sink = Sink::start("traversal", &[]);
    let mut c = sink.connect();
    assert!(c.handshake());
    c.message(
        &format!(
            r#"{{"type":"hello","token":"{TOKEN}","session":"../../escape","url":"u","title":"t"}}"#
        ),
        b"",
    );
    std::thread::sleep(Duration::from_millis(500));
    let outside: &Path = sink.out.parent().unwrap();
    assert!(!outside.join("escape").exists());
    assert!(!outside.parent().unwrap().join("escape").exists());
}

//! The host's job: carry messages between the browser's pipe and the app's
//! bridge, and refuse everything that does not fit the schema.
//!
//! The host knows no secret of the sink. It checks the origin the browser
//! names (only the streamtap extension), opens the bridge on the port the app
//! wrote and proves, with the key written next to it, that it can read the
//! app's config directory, after the listener on that port proved the same
//! (`auth`: the key is never sent, so whoever holds a dead app's port learns
//! nothing and is never taken for the app). Then it forwards the extension's
//! messages to the app and hands the app's answers back. Port and token only
//! ever arrive from the app, after the user approved the pairing there.

use std::fs;
use std::io::{self, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::framing::{read_frame, read_line, write_frame};
use crate::proto::{
    BridgeMsg, ExtMsg, HostMsg, ALLOWED_ORIGIN, APP_IDENTIFIER, BRIDGE_FILE, MAX_BRIDGE_KEY,
    MAX_BRIDGE_LINE, MAX_FROM_EXTENSION, MAX_TO_EXTENSION, MIN_BRIDGE_KEY,
};

/// Exit code: the browser named an origin other than the extension's.
pub const EXIT_ORIGIN: u8 = 2;
/// Exit code: the app is not running (no bridge to connect to).
pub const EXIT_NO_APP: u8 = 3;
/// Exit code: the extension sent something outside the schema.
pub const EXIT_PROTOCOL: u8 = 4;
/// Exit code: the pipe or the bridge failed.
pub const EXIT_IO: u8 = 5;

/// Longest wait for the app's bridge to accept.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// The app's config directory: where `bridge.json` lives. `STREAMTAP_APP_DIR`
/// overrides it (tests); otherwise it is `%APPDATA%\<identifier>`, the
/// directory the Tauri app uses for its config.
#[must_use]
pub fn app_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("STREAMTAP_APP_DIR") {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join(APP_IDENTIFIER))
}

/// Longest the handshake with the app's bridge may take in total: a listener
/// that says nothing, or a byte now and then, is not the app.
const AUTH_TIMEOUT: Duration = Duration::from_secs(2);

/// The bridge socket during the handshake: every read and write counts against
/// one deadline, so a listener that trickles its answer cannot hold the host.
struct Until<'a> {
    stream: &'a TcpStream,
    deadline: Instant,
}

impl Until<'_> {
    fn left(&self) -> io::Result<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| io::ErrorKind::TimedOut.into())
    }
}

impl Read for Until<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.set_read_timeout(Some(self.left()?))?;
        let mut stream = self.stream;
        stream.read(buf)
    }
}

impl Write for Until<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stream.set_write_timeout(Some(self.left()?))?;
        let mut stream = self.stream;
        stream.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        let mut stream = self.stream;
        stream.flush()
    }
}

/// What `bridge.json` holds: where the app listens and the key both ends prove
/// to each other on every connection (new at each app start).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bridge {
    /// The app's loopback port.
    pub port: u16,
    key: String,
}

impl Bridge {
    /// The key of the handshake (`auth`); it is proved, never sent.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }
}

impl std::fmt::Debug for Bridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Bridge(port {}, key hidden)", self.port)
    }
}

/// Reads the bridge port and key the app wrote into `dir`.
///
/// # Errors
/// When the file is missing, too large or not `{"port": n, "key": "<hex>"}`.
pub fn read_bridge(dir: &Path) -> io::Result<Bridge> {
    let mut text = String::new();
    fs::File::open(dir.join(BRIDGE_FILE))?
        .take(1024)
        .read_to_string(&mut text)?;
    let file: Bridge = serde_json::from_str(&text)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bridge file is not valid"))?;
    if file.port < 1024 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bridge port"));
    }
    if !(MIN_BRIDGE_KEY..=MAX_BRIDGE_KEY).contains(&file.key.len())
        || !file.key.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bridge key"));
    }
    Ok(file)
}

/// Connects to the bridge named in `dir/bridge.json` (loopback only, by
/// construction) and runs the handshake: the listener must prove the key of
/// that file before this end proves it in return. The key itself is not sent,
/// and nothing else is until both proofs are through.
///
/// # Errors
/// When the file is missing or stale, the app does not accept, or whoever
/// listens on that port does not prove the key within `AUTH_TIMEOUT` in total
/// (a foreign process on the port of an app that is gone): no connection is
/// handed out then.
pub fn connect_with(dir: &Path) -> io::Result<TcpStream> {
    let bridge = read_bridge(dir)?;
    let stream = TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], bridge.port)),
        CONNECT_TIMEOUT,
    )?;
    let mut timed = Until {
        stream: &stream,
        deadline: Instant::now() + AUTH_TIMEOUT,
    };
    if let Err(e) = crate::auth::host_handshake(&mut timed, bridge.key()) {
        let _ = stream.shutdown(Shutdown::Both);
        return Err(e);
    }
    stream.set_write_timeout(None)?;
    stream.set_read_timeout(None)?;
    Ok(stream)
}

/// Connects to the app's bridge (see [`connect_with`]).
///
/// # Errors
/// When the app is not running or the file is stale.
pub fn connect_to_app() -> io::Result<TcpStream> {
    let dir = app_dir().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no app dir"))?;
    connect_with(&dir)
}

fn send<W: Write>(out: &Mutex<W>, msg: &HostMsg) -> io::Result<()> {
    let bytes = msg.to_bytes().map_err(io::Error::other)?;
    let mut w = out.lock().unwrap_or_else(PoisonError::into_inner);
    write_frame(&mut *w, &bytes, MAX_TO_EXTENSION)
}

fn send_bridge(stream: &mut impl Write, msg: &BridgeMsg) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(msg).map_err(io::Error::other)?;
    bytes.push(b'\n');
    stream.write_all(&bytes)?;
    stream.flush()
}

enum Next {
    Msg(ExtMsg),
    End,
    Bad(&'static str),
}

fn next_ext(input: &mut impl Read) -> Next {
    match read_frame(input, MAX_FROM_EXTENSION) {
        Ok(None) => Next::End,
        Err(_) => Next::Bad("bad-frame"),
        Ok(Some(bytes)) => ExtMsg::parse(&bytes).map_or(Next::Bad("bad-message"), Next::Msg),
    }
}

/// Runs the host until the browser closes the pipe or something is refused.
/// Returns the process exit code. `on_bridge_closed` is called (on another
/// thread) when the app closes the bridge first; the binary exits then.
pub fn run<R, W>(
    origin: Option<&str>,
    mut input: R,
    output: W,
    connect: impl FnOnce() -> io::Result<TcpStream>,
    on_bridge_closed: impl Fn() + Send + 'static,
) -> u8
where
    R: Read,
    W: Write + Send + 'static,
{
    let out = Arc::new(Mutex::new(output));
    // The origin is the browser's word: it is the only identity the host has.
    let Some(origin) = origin.filter(|o| *o == ALLOWED_ORIGIN) else {
        let _ = send(&out, &HostMsg::error("origin-not-allowed"));
        return EXIT_ORIGIN;
    };
    let hello = match next_ext(&mut input) {
        Next::Msg(m @ ExtMsg::Hello { .. }) => m,
        Next::Msg(_) => {
            let _ = send(&out, &HostMsg::error("expected-hello"));
            return EXIT_PROTOCOL;
        }
        Next::End => return 0,
        Next::Bad(code) => {
            let _ = send(&out, &HostMsg::error(code));
            return EXIT_PROTOCOL;
        }
    };
    let Ok(mut bridge) = connect() else {
        let _ = send(&out, &HostMsg::error("app-not-running"));
        return EXIT_NO_APP;
    };
    let Ok(reader) = bridge.try_clone() else {
        return EXIT_IO;
    };
    let closing = Arc::new(AtomicBool::new(false));
    {
        let (out, closing) = (Arc::clone(&out), Arc::clone(&closing));
        std::thread::spawn(move || {
            pump_from_app(reader, &out);
            if !closing.load(Ordering::SeqCst) {
                let _ = send(&out, &HostMsg::error("app-closed"));
                on_bridge_closed();
            }
        });
    }
    if send_bridge(&mut bridge, &hello.into_bridge(origin)).is_err() {
        return EXIT_IO;
    }
    let code = loop {
        match next_ext(&mut input) {
            Next::End => break 0,
            Next::Bad(code) => {
                let _ = send(&out, &HostMsg::error(code));
                break EXIT_PROTOCOL;
            }
            Next::Msg(ExtMsg::Hello { .. }) => {
                let _ = send(&out, &HostMsg::error("unexpected-hello"));
                break EXIT_PROTOCOL;
            }
            Next::Msg(m) => {
                if send_bridge(&mut bridge, &m.into_bridge(origin)).is_err() {
                    break EXIT_IO;
                }
            }
        }
    };
    closing.store(true, Ordering::SeqCst);
    let _ = bridge.shutdown(Shutdown::Both);
    code
}

/// App -> extension until the bridge ends or the app says something invalid.
fn pump_from_app<W: Write>(stream: TcpStream, out: &Mutex<W>) {
    let mut lines = BufReader::new(stream);
    while let Ok(Some(line)) = read_line(&mut lines, MAX_BRIDGE_LINE) {
        if let Ok(msg) = HostMsg::parse(&line) {
            if send(out, &msg).is_err() {
                return;
            }
        } else {
            let _ = send(out, &HostMsg::error("bad-app-message"));
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::Pairing;
    use std::io::Cursor;
    use std::net::TcpListener;

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl Buf {
        fn messages(&self) -> Vec<HostMsg> {
            let data = self.0.lock().unwrap().clone();
            let mut r = Cursor::new(data);
            let mut out = Vec::new();
            while let Some(b) = read_frame(&mut r, MAX_TO_EXTENSION).unwrap() {
                out.push(HostMsg::parse(&b).unwrap());
            }
            out
        }
    }

    fn frame(json: &str) -> Vec<u8> {
        let mut v = u32::try_from(json.len()).unwrap().to_le_bytes().to_vec();
        v.extend_from_slice(json.as_bytes());
        v
    }

    const HELLO: &str =
        r#"{"t":"hello","v":1,"instance":"abcd1234-ef","browser":"edge","version":"0.3.0"}"#;

    fn no_connect() -> io::Result<TcpStream> {
        panic!("must not connect")
    }

    #[test]
    fn another_origin_is_refused_before_anything_is_read_or_connected() {
        for origin in [
            None,
            Some("chrome-extension://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/"),
            Some("chrome-extension://oggpddakdbffbjhkjmcakidgmhpgakia"),
            Some("https://example.com/"),
            Some(""),
        ] {
            let out = Buf::default();
            let code = run(
                origin,
                Cursor::new(frame(HELLO)),
                out.clone(),
                no_connect,
                || {},
            );
            assert_eq!(code, EXIT_ORIGIN, "{origin:?}");
            assert_eq!(out.messages(), [HostMsg::error("origin-not-allowed")]);
        }
    }

    #[test]
    fn a_first_message_other_than_hello_is_refused() {
        let out = Buf::default();
        let code = run(
            Some(ALLOWED_ORIGIN),
            Cursor::new(frame(r#"{"t":"ping","v":1}"#)),
            out.clone(),
            no_connect,
            || {},
        );
        assert_eq!(code, EXIT_PROTOCOL);
        assert_eq!(out.messages(), [HostMsg::error("expected-hello")]);
    }

    #[test]
    fn an_oversized_or_malformed_frame_ends_the_host() {
        let out = Buf::default();
        let huge = u32::try_from(MAX_FROM_EXTENSION + 1)
            .unwrap()
            .to_le_bytes()
            .to_vec();
        let code = run(
            Some(ALLOWED_ORIGIN),
            Cursor::new(huge),
            out.clone(),
            no_connect,
            || {},
        );
        assert_eq!(code, EXIT_PROTOCOL);
        assert_eq!(out.messages(), [HostMsg::error("bad-frame")]);
        let out = Buf::default();
        let code = run(
            Some(ALLOWED_ORIGIN),
            Cursor::new(frame(r#"{"t":"hello","v":1,"extra":1}"#)),
            out.clone(),
            no_connect,
            || {},
        );
        assert_eq!(code, EXIT_PROTOCOL);
        assert_eq!(out.messages(), [HostMsg::error("bad-message")]);
    }

    #[test]
    fn without_the_app_the_extension_is_told_so() {
        let out = Buf::default();
        let code = run(
            Some(ALLOWED_ORIGIN),
            Cursor::new(frame(HELLO)),
            out.clone(),
            || Err(io::ErrorKind::ConnectionRefused.into()),
            || {},
        );
        assert_eq!(code, EXIT_NO_APP);
        assert_eq!(out.messages(), [HostMsg::error("app-not-running")]);
    }

    /// Runs the host against a fake app on a real loopback socket. The fake
    /// app reads the lines the host sends and answers with `reply` after the
    /// hello.
    fn with_fake_app(input: Vec<u8>, reply: &'static [u8]) -> (u8, Buf, Vec<String>) {
        /// The browser keeps the pipe open a while before it closes it.
        struct Lingering(Cursor<Vec<u8>>);
        impl Read for Lingering {
            fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
                let n = self.0.read(b)?;
                if n == 0 {
                    std::thread::sleep(Duration::from_millis(300));
                }
                Ok(n)
            }
        }
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut lines = BufReader::new(s.try_clone().unwrap());
            let mut got = Vec::new();
            let hello = read_line(&mut lines, 1 << 20).unwrap().unwrap();
            got.push(String::from_utf8(hello).unwrap());
            s.write_all(reply).unwrap();
            while let Ok(Some(l)) = read_line(&mut lines, 1 << 20) {
                got.push(String::from_utf8(l).unwrap());
            }
            got
        });
        let out = Buf::default();
        let code = run(
            Some(ALLOWED_ORIGIN),
            Lingering(Cursor::new(input)),
            out.clone(),
            move || TcpStream::connect(("127.0.0.1", port)),
            || {},
        );
        let got = app.join().unwrap();
        (code, out, got)
    }

    #[test]
    fn messages_flow_both_ways_and_the_hello_carries_the_origin() {
        let mut input = frame(HELLO);
        input.extend(frame(r#"{"t":"ping","v":1}"#));
        let reply = b"{\"t\":\"state\",\"v\":1,\"pairing\":\"pending\"}\n{\"t\":\"tap\",\"v\":1,\"tab\":3,\"on\":true}\n";
        let (code, out, got) = with_fake_app(input, reply);
        assert_eq!(code, 0);
        assert_eq!(got.len(), 2);
        let BridgeMsg::Hello {
            origin, browser, ..
        } = BridgeMsg::parse(got[0].as_bytes()).unwrap()
        else {
            panic!("hello first")
        };
        assert_eq!(
            (origin.as_str(), browser.as_str()),
            (ALLOWED_ORIGIN, "edge")
        );
        assert!(matches!(
            BridgeMsg::parse(got[1].as_bytes()).unwrap(),
            BridgeMsg::Ping { .. }
        ));
        assert_eq!(
            out.messages(),
            [HostMsg::state(Pairing::Pending), HostMsg::tap(3, true)]
        );
    }

    #[test]
    fn an_invalid_answer_from_the_app_is_not_passed_on() {
        let (_, out, _) = with_fake_app(frame(HELLO), b"{\"t\":\"shell\",\"cmd\":\"calc\"}\n");
        std::thread::sleep(Duration::from_millis(200));
        // Nothing of the junk is passed on; the extension is told, then the link is over.
        assert_eq!(
            out.messages(),
            [
                HostMsg::error("bad-app-message"),
                HostMsg::error("app-closed")
            ]
        );
    }

    const KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    fn bridge_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "streamtap-host-bridge-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_bridge_port_and_key_come_from_a_small_strict_file() {
        let dir = bridge_dir("file");
        assert!(read_bridge(&dir).is_err(), "missing");
        fs::write(
            dir.join(BRIDGE_FILE),
            format!(r#"{{"port":40123,"key":"{KEY}"}}"#),
        )
        .unwrap();
        let b = read_bridge(&dir).unwrap();
        assert_eq!((b.port, b.key()), (40123, KEY));
        assert!(!format!("{b:?}").contains(KEY), "the key is not printed");
        for bad in [
            format!(r#"{{"port":80,"key":"{KEY}"}}"#),
            format!(r#"{{"port":40123,"key":"{KEY}","x":1}}"#),
            format!(r#"{{"port":"40123","key":"{KEY}"}}"#),
            r#"{"port":40123}"#.to_string(),
            r#"{"port":40123,"key":"abc"}"#.to_string(),
            format!(
                r#"{{"port":40123,"key":"{}"}}"#,
                "a".repeat(MAX_BRIDGE_KEY + 1)
            ),
            format!(r#"{{"port":40123,"key":"{}"}}"#, "g".repeat(64)),
            format!(r#"{{"port":40123,"key":"{}\n"}}"#, &KEY[..63]),
            "nope".to_string(),
        ] {
            fs::write(dir.join(BRIDGE_FILE), &bad).unwrap();
            assert!(read_bridge(&dir).is_err(), "{bad}");
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    /// What a listener on the bridge port does with a connection; returns every byte it read.
    type Listener = Box<dyn FnOnce(TcpStream) -> Vec<u8> + Send>;

    /// A `bridge.json` with `KEY` that names a port `listener` answers on (one connection).
    fn bridge_with(name: &str, listener: Listener) -> (PathBuf, std::thread::JoinHandle<Vec<u8>>) {
        let dir = bridge_dir(name);
        let socket = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = socket.local_addr().unwrap().port();
        fs::write(
            dir.join(BRIDGE_FILE),
            format!(r#"{{"port":{port},"key":"{KEY}"}}"#),
        )
        .unwrap();
        let heard = std::thread::spawn(move || {
            let (s, _) = socket.accept().unwrap();
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            listener(s)
        });
        (dir, heard)
    }

    /// The app's side of the handshake with `key`, then `after`; everything read is returned.
    fn app_with(key: &'static str, after: &'static [u8]) -> Listener {
        Box::new(move |mut s| {
            use crate::auth;
            let mut heard = Vec::new();
            let mut lines = BufReader::new(s.try_clone().unwrap());
            let greeting = read_line(&mut lines, 1024)
                .ok()
                .flatten()
                .unwrap_or_default();
            heard.extend_from_slice(&greeting);
            heard.push(b'\n');
            let Some(host_nonce) = auth::parse_greeting(&greeting) else {
                return heard;
            };
            let app_nonce = auth::nonce().unwrap();
            s.write_all(auth::answer_line(key, host_nonce, &app_nonce).as_bytes())
                .unwrap();
            if let Some(proof) = read_line(&mut lines, 1024).ok().flatten() {
                let text = String::from_utf8_lossy(&proof).into_owned();
                heard.extend_from_slice(&proof);
                heard.push(b'\n');
                if auth::proves(key, auth::Role::Host, host_nonce, &app_nonce, &text) {
                    heard.extend_from_slice(b"HOST-PROVEN\n");
                    let _ = s.write_all(after);
                }
            }
            // Whatever else arrives (a hello must not, after a failed handshake).
            let _ = lines.read_to_end(&mut heard);
            heard
        })
    }

    #[test]
    fn host_and_app_prove_the_key_and_the_key_is_never_sent() {
        let (dir, heard) = bridge_with("proven", app_with(KEY, b""));
        let s = connect_with(&dir).expect("both know the key");
        drop(s);
        let heard = String::from_utf8(heard.join().unwrap()).unwrap();
        assert!(heard.contains("HOST-PROVEN"), "{heard}");
        assert!(heard.starts_with(crate::auth::GREETING), "{heard}");
        assert!(!heard.contains(KEY), "the key crossed the socket");
        // Without a usable file nothing is connected.
        fs::write(dir.join(BRIDGE_FILE), r#"{"port":40123}"#).unwrap();
        assert!(connect_with(&dir).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The app is gone and `bridge.json` stayed (a crash); another local process listens on the old port. Whatever
    /// it answers, the host hands it nothing: not the key, no proof made with the key, not the extension's hello.
    /// And nothing it says reaches the extension: the host reports that the app is not running.
    #[test]
    fn a_foreign_listener_on_the_old_port_gets_no_secret_and_is_not_taken_for_the_app() {
        const FORGED: &[u8] = b"{\"t\":\"state\",\"v\":1,\"pairing\":\"approved\"}\n\
            {\"t\":\"credentials\",\"v\":1,\"port\":40999,\"token\":\"attackers-own-sink-token\"}\n";
        // What the stranger answers to the greeting, then it sends its forged credentials and reads on.
        let answers: Vec<(&str, Vec<u8>)> = vec![
            ("nothing but its own messages", Vec::new()),
            ("the greeting echoed", b"ECHO".to_vec()),
            (
                "a nonce and a proof made without the key",
                format!(
                    "{0} {1}\n",
                    "ab".repeat(32),
                    crate::auth::proof(
                        &"cd".repeat(32),
                        crate::auth::Role::App,
                        &"ef".repeat(32),
                        &"ab".repeat(32)
                    )
                )
                .into_bytes(),
            ),
            (
                "a nonce without a proof",
                format!("{}\n", "ab".repeat(32)).into_bytes(),
            ),
            ("a very long line", vec![b'a'; 8192]),
        ];
        for (what, answer) in answers {
            let stranger: Listener = Box::new(move |mut s| {
                let mut heard = Vec::new();
                let mut lines = BufReader::new(s.try_clone().unwrap());
                let greeting = read_line(&mut lines, 1024)
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                heard.extend_from_slice(&greeting);
                heard.push(b'\n');
                let answer = if answer == b"ECHO" {
                    [&greeting[..], b"\n"].concat()
                } else {
                    answer
                };
                let _ = s.write_all(&answer);
                let _ = s.write_all(FORGED);
                let _ = lines.read_to_end(&mut heard);
                heard
            });
            let (dir, heard) = bridge_with("stranger", stranger);
            let out = Buf::default();
            let connect_dir = dir.clone();
            let code = run(
                Some(ALLOWED_ORIGIN),
                Cursor::new(frame(HELLO)),
                out.clone(),
                move || connect_with(&connect_dir),
                || panic!("no bridge was ever open"),
            );
            assert_eq!(code, EXIT_NO_APP, "{what}");
            assert_eq!(
                out.messages(),
                [HostMsg::error("app-not-running")],
                "{what}: nothing of the stranger's reaches the extension"
            );
            let heard = String::from_utf8(heard.join().unwrap()).unwrap();
            let nonce = crate::auth::parse_greeting(heard.trim_end().as_bytes());
            assert!(
                nonce.is_some(),
                "{what}: the stranger heard more than one greeting: {heard:?}"
            );
            assert!(!heard.contains(KEY), "{what}: the key was sent");
            assert!(!heard.contains("hello"), "{what}: the hello was sent");
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    /// A listener that accepts and then says nothing is not the app either: the host gives up after its time
    /// limit instead of waiting for ever, and has sent nothing but the greeting.
    #[test]
    fn a_silent_listener_is_given_up_after_the_time_limit() {
        let silent: Listener = Box::new(|s| {
            let mut heard = Vec::new();
            let _ = BufReader::new(s).read_to_end(&mut heard);
            heard
        });
        let (dir, heard) = bridge_with("silent", silent);
        let started = Instant::now();
        let err = connect_with(&dir).expect_err("no proof");
        assert!(
            matches!(
                err.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ),
            "{err:?}"
        );
        assert!(
            started.elapsed() < AUTH_TIMEOUT * 3,
            "{:?}",
            started.elapsed()
        );
        let heard = String::from_utf8(heard.join().unwrap()).unwrap();
        assert!(
            crate::auth::parse_greeting(heard.trim_end().as_bytes()).is_some(),
            "{heard:?}"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The time limit counts for the whole handshake: a listener that sends a byte of an answer now and then (each
    /// read is quick) is given up all the same, long before its line could be complete.
    #[test]
    fn a_trickling_listener_is_given_up_after_the_time_limit() {
        let trickle: Listener = Box::new(|mut s| {
            let mut heard = Vec::new();
            let mut lines = BufReader::new(s.try_clone().unwrap());
            heard.extend(
                read_line(&mut lines, 1024)
                    .ok()
                    .flatten()
                    .unwrap_or_default(),
            );
            // 130 bytes at one every 400 ms would take 52 s.
            for _ in 0..130 {
                if s.write_all(b"a").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(400));
            }
            heard
        });
        let (dir, heard) = bridge_with("trickle", trickle);
        let started = Instant::now();
        let err = connect_with(&dir).expect_err("no proof");
        assert!(
            matches!(
                err.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ),
            "{err:?}"
        );
        assert!(
            started.elapsed() < AUTH_TIMEOUT * 2,
            "given up only after {:?}",
            started.elapsed()
        );
        heard.join().unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The real app's answer from an earlier connection proves nothing on a new one: the host's nonce is fresh.
    #[test]
    fn a_replayed_answer_of_the_app_is_refused() {
        // Record what an app that knows the key answers.
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let first_app: Listener = Box::new(move |mut s| {
            let mut lines = BufReader::new(s.try_clone().unwrap());
            let greeting = read_line(&mut lines, 1024).unwrap().unwrap();
            let host_nonce = crate::auth::parse_greeting(&greeting).unwrap().to_owned();
            let answer = crate::auth::answer_line(KEY, &host_nonce, &crate::auth::nonce().unwrap());
            s.write_all(answer.as_bytes()).unwrap();
            tx.send(answer.into_bytes()).unwrap();
            Vec::new()
        });
        let (dir, heard) = bridge_with("record", first_app);
        drop(connect_with(&dir).expect("the real app"));
        heard.join().unwrap();
        fs::remove_dir_all(&dir).unwrap();
        let recorded = rx.recv().unwrap();

        let replayer: Listener = Box::new(move |mut s| {
            let mut heard = Vec::new();
            let mut lines = BufReader::new(s.try_clone().unwrap());
            heard.extend(
                read_line(&mut lines, 1024)
                    .ok()
                    .flatten()
                    .unwrap_or_default(),
            );
            heard.push(b'\n');
            let _ = s.write_all(&recorded);
            let _ = lines.read_to_end(&mut heard);
            heard
        });
        let (dir, heard) = bridge_with("replay", replayer);
        let err = connect_with(&dir).expect_err("a stale proof");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        let heard = String::from_utf8(heard.join().unwrap()).unwrap();
        assert!(
            crate::auth::parse_greeting(heard.trim_end().as_bytes()).is_some(),
            "the host answered a replay: {heard:?}"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The app wrote another key than the file the host read (a stale or forged `bridge.json`): the host does not
    /// take it for its app, and the app is not shown the host's proof.
    #[test]
    fn an_app_with_another_key_is_not_the_app() {
        const OTHER: &str = "ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100";
        let (dir, heard) = bridge_with("otherkey", app_with(OTHER, b""));
        let err = connect_with(&dir).expect_err("another key");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        let heard = String::from_utf8(heard.join().unwrap()).unwrap();
        assert!(!heard.contains("HOST-PROVEN"));
        assert!(
            crate::auth::parse_greeting(heard.trim_end().as_bytes()).is_some(),
            "{heard:?}"
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}

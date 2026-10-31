//! The host's job: carry messages between the browser's pipe and the app's
//! bridge, and refuse everything that does not fit the schema.
//!
//! The host knows no secret. It checks the origin the browser names (only the
//! streamtap extension), forwards the extension's messages to the app, and
//! hands the app's answers back. Port and token only ever arrive from the app,
//! after the user approved the pairing there.

use std::fs;
use std::io::{self, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde::Deserialize;

use crate::framing::{read_frame, read_line, write_frame};
use crate::proto::{
    BridgeMsg, ExtMsg, HostMsg, ALLOWED_ORIGIN, APP_IDENTIFIER, BRIDGE_FILE, MAX_BRIDGE_LINE,
    MAX_FROM_EXTENSION, MAX_TO_EXTENSION,
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BridgeFile {
    port: u16,
}

/// Reads the bridge port the app wrote into `dir`.
///
/// # Errors
/// When the file is missing, too large or not `{"port": n}`.
pub fn read_bridge_port(dir: &Path) -> io::Result<u16> {
    let mut text = String::new();
    fs::File::open(dir.join(BRIDGE_FILE))?
        .take(1024)
        .read_to_string(&mut text)?;
    let file: BridgeFile = serde_json::from_str(&text)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bridge file is not valid"))?;
    if file.port < 1024 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bridge port"));
    }
    Ok(file.port)
}

/// Connects to the app's bridge (loopback only, by construction).
///
/// # Errors
/// When the app is not running or the file is stale.
pub fn connect_to_app() -> io::Result<TcpStream> {
    let dir = app_dir().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no app dir"))?;
    let port = read_bridge_port(&dir)?;
    TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), CONNECT_TIMEOUT)
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

    #[test]
    fn the_bridge_port_comes_from_a_small_strict_file() {
        let dir =
            std::env::temp_dir().join(format!("streamtap-host-bridge-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        assert!(read_bridge_port(&dir).is_err(), "missing");
        fs::write(dir.join(BRIDGE_FILE), br#"{"port":40123}"#).unwrap();
        assert_eq!(read_bridge_port(&dir).unwrap(), 40123);
        for bad in [
            r#"{"port":80}"#,
            r#"{"port":40123,"x":1}"#,
            r#"{"port":"40123"}"#,
            "nope",
        ] {
            fs::write(dir.join(BRIDGE_FILE), bad).unwrap();
            assert!(read_bridge_port(&dir).is_err(), "{bad}");
        }
        fs::remove_dir_all(&dir).unwrap();
    }
}

//! Security tests of the sink's tailnet door (`MTST-01`, `MOB-01`, `MOB-04`, `MSEC-04`): phones through Tailscale Serve.
//!
//! The real binary is started on port 0 with `--tailnet`, its captures and its list of devices under the system temp
//! folder. No tailnet is needed: Tailscale Serve forwards to loopback and what tells its connections apart are the
//! `Host` header and the headers a proxy adds, which a test sets itself. A device's key is made here and signs the
//! challenge as `WebCrypto` does (ECDSA P-256 over SHA-256, r and s as hex).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use serde_json::{json, Value};
use tungstenite::client::IntoClientRequest;
use tungstenite::http::HeaderValue;
use tungstenite::{Message, WebSocket};

const TOKEN: &str = "tok-test-123";
/// The origin of the desktop extension (`--allow-origin`).
const ORIGIN: &str = "http://test.invalid";
/// The origin of the add-on on a phone: Firefox gives every installation its own.
const PHONE: &str = "moz-extension://0f3c1a52-7b1e-4d0c-9a3e-2f6f1f0e9c11";
const NAME: &str = "pc.example.ts.net";

struct Sink {
    child: Child,
    port: u16,
    out: PathBuf,
}

impl Sink {
    fn start(name: &str, extra: &[&str]) -> Self {
        let out = std::env::temp_dir().join(format!(
            "streamtap-tailnet-test-{}-{name}",
            std::process::id()
        ));
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

    fn tailnet(name: &str) -> Self {
        Self::start(name, &["--tailnet", NAME])
    }

    /// `streamtap-sink devices <args>` against this sink: exit code and standard output.
    fn devices(&self, args: &[&str], token: &str) -> (i32, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_streamtap-sink"))
            .arg("devices")
            .args(args)
            .args(["--token", token, "--allow-origin", ORIGIN, "--port"])
            .arg(self.port.to_string())
            .stderr(Stdio::null())
            .output()
            .expect("run devices");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    }

    /// A fresh pairing code, as the command line shows it.
    fn code(&self) -> String {
        let (status, text) = self.devices(&["pair"], TOKEN);
        assert_eq!(status, 0, "devices pair: {text}");
        assert!(
            text.contains(&format!("https://{NAME}/#streamtap-pair=1&code=")),
            "{text}"
        );
        text.lines()
            .find_map(|l| l.trim().strip_prefix("code: "))
            .expect("the code line")
            .to_owned()
    }

    /// The capture folders (the list of devices lies beside them).
    fn captures(&self) -> Vec<String> {
        std::fs::read_dir(&self.out)
            .map(|d| {
                d.flatten()
                    .filter(|e| e.path().is_dir())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// A WebSocket as Tailscale Serve hands one on: the tailnet name as `Host`, the proxy's headers, the add-on's origin.
    fn phone(&self) -> Ws {
        self.connect(NAME, PHONE, true)
            .expect("a phone's handshake")
    }

    fn connect(
        &self,
        host: &str,
        origin: &str,
        forwarded: bool,
    ) -> Result<Ws, Box<tungstenite::Error>> {
        let stream = TcpStream::connect(("127.0.0.1", self.port)).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let mut req = format!("ws://127.0.0.1:{}/", self.port)
            .into_client_request()
            .unwrap();
        let headers = req.headers_mut();
        headers.insert("host", HeaderValue::from_str(host).unwrap());
        headers.insert("origin", HeaderValue::from_str(origin).unwrap());
        if forwarded {
            headers.insert("x-forwarded-for", HeaderValue::from_static("100.64.0.7"));
            headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        }
        tungstenite::client(req, stream)
            .map(|(ws, _)| Ws(ws))
            .map_err(|e| match e {
                tungstenite::HandshakeError::Failure(e) => Box::new(e),
                tungstenite::HandshakeError::Interrupted(_) => {
                    panic!("a blocking handshake was interrupted")
                }
            })
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.out);
    }
}

struct Ws(WebSocket<TcpStream>);

impl Ws {
    /// One message in the sink's framing: header length, JSON header, payload.
    fn say(&mut self, header: &Value, payload: &[u8]) {
        let h = header.to_string().into_bytes();
        let mut frame = u32::try_from(h.len()).unwrap().to_le_bytes().to_vec();
        frame.extend_from_slice(&h);
        frame.extend_from_slice(payload);
        // A sink that already closed is what some tests are about.
        let _ = self.0.send(Message::binary(frame));
    }

    /// What the sink says next (JSON text); `Null` when it closed without a word.
    fn hear(&mut self) -> Value {
        loop {
            match self.0.read() {
                Ok(Message::Text(t)) => {
                    return serde_json::from_str(t.as_str()).expect("JSON from the sink")
                }
                Ok(Message::Close(_)) | Err(_) => return Value::Null,
                Ok(_) => {}
            }
        }
    }

    /// True once the sink closed the connection (or dropped it).
    fn closed(&mut self) -> bool {
        self.hear().is_null()
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// A device: its key (the private half never leaves this struct) and what the sink said at pairing.
struct Device {
    key: SigningKey,
    id: String,
    hub: String,
}

impl Device {
    fn key(seed: u8) -> (SigningKey, String) {
        let key = SigningKey::from_slice(&[seed; 32]).unwrap();
        let public = hex(key.verifying_key().to_sec1_point(false).as_bytes());
        (key, public)
    }

    fn pair(sink: &Sink, seed: u8) -> Self {
        let (key, public) = Self::key(seed);
        let mut ws = sink.phone();
        ws.say(&json!({ "type": "pair", "v": 1, "code": sink.code(), "name": "test phone", "key": public }), b"");
        let said = ws.hear();
        assert_eq!(said["type"], "paired", "{said}");
        Self {
            key,
            id: said["device"].as_str().unwrap().to_owned(),
            hub: said["hub"].as_str().unwrap().to_owned(),
        }
    }

    fn sign(&self, origin: &str, challenge: &str) -> String {
        let message = format!("appkit-device-login/1\n{origin}\n{}\n{challenge}", self.id);
        let sig: Signature = self.key.sign(message.as_bytes());
        hex(&sig.to_bytes())
    }

    /// `login` and `auth`; what the sink answered last, and the socket.
    fn login(&self, sink: &Sink) -> (Value, Ws) {
        let mut ws = sink.phone();
        ws.say(&json!({ "type": "login", "v": 1, "device": self.id }), b"");
        let asked = ws.hear();
        assert_eq!(asked["type"], "challenge", "{asked}");
        assert_eq!(
            asked["hub"],
            self.hub.as_str(),
            "the sink names itself before anything is signed"
        );
        let signature = self.sign(
            &format!("https://{NAME}"),
            asked["challenge"].as_str().unwrap(),
        );
        ws.say(&json!({ "type": "auth", "signature": signature }), b"");
        (ws.hear(), ws)
    }
}

fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    let until = std::time::Instant::now() + Duration::from_secs(20);
    while !ok() {
        assert!(std::time::Instant::now() < until, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_paired_phone_logs_in_with_its_key_and_captures_without_the_token() {
    let sink = Sink::tailnet("happy");
    let device = Device::pair(&sink, 7);
    let (said, mut ws) = device.login(&sink);
    assert_eq!(said["type"], "ok", "{said}");
    ws.say(
        &json!({ "type": "hello", "session": "s1", "url": "https://example.org/v", "title": "t" }),
        b"",
    );
    wait_for("the capture folder of the device's session", || {
        !sink.captures().is_empty()
    });

    let (status, list) = sink.devices(&[], TOKEN);
    assert_eq!(status, 0);
    assert!(
        list.contains(&device.id) && list.contains("test phone"),
        "{list}"
    );
    assert!(list.contains("open connections 1"), "{list}");
}

/// `--json` is what the desktop app reads: one line, the pairing with its link and a square QR code.
#[test]
fn the_command_line_answers_a_program_in_json() {
    let sink = Sink::tailnet("json");
    let (status, text) = sink.devices(&["pair", "--json"], TOKEN);
    assert_eq!(status, 0, "{text}");
    assert_eq!(text.trim().lines().count(), 1, "{text}");
    let pairing: Value = serde_json::from_str(&text).expect("JSON");
    let code = pairing["code"].as_str().expect("code");
    assert_eq!(pairing["tailnet"], NAME);
    assert_eq!(pairing["ttl"], 60);
    assert_eq!(
        pairing["link"],
        format!("https://{NAME}/#streamtap-pair=1&code={code}").as_str()
    );
    let rows = pairing["qr"].as_array().expect("qr");
    assert!(rows.len() >= 21, "a QR code has at least 21 modules a side");
    for row in rows {
        let row = row.as_str().expect("a row");
        assert_eq!(row.len(), rows.len(), "the code is square");
        assert!(row.bytes().all(|c| c == b'0' || c == b'1'));
    }
    // The code of that answer pairs a phone, and the list names it.
    let (_, public) = Device::key(11);
    let mut ws = sink.phone();
    ws.say(
        &json!({ "type": "pair", "v": 1, "code": code, "name": "json phone", "key": public }),
        b"",
    );
    let id = ws.hear()["device"].as_str().expect("paired").to_owned();
    let (status, text) = sink.devices(&["--json"], TOKEN);
    assert_eq!(status, 0);
    let list: Value = serde_json::from_str(&text).expect("JSON");
    let device = &list["devices"][0];
    assert_eq!(device["id"], id.as_str());
    assert_eq!(device["name"], "json phone");
    assert_eq!(device["level"], "create");
    assert_eq!(device["open"], 0);
    assert!(device["paired"].as_u64().is_some() && device["last_used"].is_null());
    assert_eq!(sink.devices(&["revoke", &id, "--json"], TOKEN).0, 0);
    assert_eq!(sink.devices(&["revoke", &id, "--json"], TOKEN).0, 2, "gone");
    assert_eq!(sink.devices(&["--json"], "wrong-token").0, 1);
}

#[test]
fn the_list_of_devices_holds_no_secret() {
    let sink = Sink::tailnet("file");
    let device = Device::pair(&sink, 9);
    let text = std::fs::read_to_string(sink.out.join("devices.json"))
        .expect("devices.json beside the captures");
    let public = hex(device.key.verifying_key().to_sec1_point(false).as_bytes());
    assert!(
        text.contains(&public),
        "the public key is what the sink keeps"
    );
    assert!(
        !text.contains(&hex(&device.key.to_bytes())),
        "never the private key"
    );
    assert!(!text.contains(TOKEN));
}

#[test]
fn the_shared_token_opens_nothing_through_the_tailnet() {
    let sink = Sink::tailnet("token");
    let mut ws = sink.phone();
    ws.say(
        &json!({ "type": "hello", "token": TOKEN, "session": "s", "url": "u", "title": "t" }),
        b"",
    );
    assert_eq!(ws.hear()["reason"], "protocol");
    // The command line of the sink is for this machine only, whatever token comes with it.
    let mut ws = sink.phone();
    ws.say(
        &json!({ "type": "control", "token": TOKEN, "op": "pair" }),
        b"",
    );
    assert_eq!(ws.hear()["reason"], "protocol");
    assert_eq!(sink.captures(), Vec::<String>::new());
}

#[test]
fn a_web_page_and_a_forged_host_stop_at_the_handshake() {
    let sink = Sink::tailnet("origin");
    // A page open on the phone (any site can try the tailnet name).
    for origin in [
        "https://evil.example",
        "null",
        "moz-extension://",
        "moz-extension://UPPER",
        "chrome-extension://a/b",
    ] {
        assert!(
            sink.connect(NAME, origin, true).is_err(),
            "origin {origin} must be refused"
        );
    }
    // Through the proxy but claiming to be local: the right origin and the token do not help.
    assert!(sink
        .connect(&format!("127.0.0.1:{}", sink.port), ORIGIN, true)
        .is_err());
    assert!(sink.connect("other.example.ts.net", PHONE, true).is_err());
    // Local connections are as they were.
    assert!(sink
        .connect(&format!("127.0.0.1:{}", sink.port), ORIGIN, false)
        .is_ok());
}

#[test]
fn without_the_flag_nothing_forwarded_gets_in() {
    let mut sink = Sink::start("noflag", &[]);
    assert!(sink.connect(NAME, PHONE, true).is_err());
    assert!(sink
        .connect(&format!("127.0.0.1:{}", sink.port), ORIGIN, true)
        .is_err());
    let (status, _) = sink.devices(&["pair"], TOKEN);
    assert_eq!(
        status, 1,
        "a sink without --tailnet says that it has no devices"
    );
    assert!(sink.alive());
}

#[test]
fn a_pairing_code_works_once_and_only_with_a_good_key() {
    let sink = Sink::tailnet("code");
    let code = sink.code();
    let (_, public) = Device::key(3);
    let pair = |code: &str, name: &str, key: &str, v: u32| {
        let mut ws = sink.phone();
        ws.say(
            &json!({ "type": "pair", "v": v, "code": code, "name": name, "key": key }),
            b"",
        );
        ws.hear()
    };
    assert_eq!(pair(&"0".repeat(32), "p", &public, 1)["reason"], "code");
    assert_eq!(pair(&code, "p", &public, 2)["reason"], "version");
    // A bad key does not spend the code.
    assert_eq!(pair(&code, "p", "04abcd", 1)["reason"], "key");
    assert_eq!(pair(&code, "p", &public, 1)["type"], "paired");
    let (_, other) = Device::key(4);
    assert_eq!(
        pair(&code, "p", &other, 1)["reason"],
        "code",
        "a code is good for one device"
    );
    assert_eq!(
        pair(&sink.code(), "p", &public, 1)["reason"],
        "known",
        "a key pairs once"
    );
}

#[test]
fn a_wrong_signature_an_unknown_device_and_a_stale_challenge_are_refused() {
    let sink = Sink::tailnet("login");
    let device = Device::pair(&sink, 5);
    let other = Device {
        key: Device::key(6).0,
        id: device.id.clone(),
        hub: device.hub.clone(),
    };
    assert_eq!(
        other.login(&sink).0["reason"],
        "device",
        "another key for the same id"
    );

    // Signed for another origin: a signature made for one sink is of no use at another.
    let mut ws = sink.phone();
    ws.say(
        &json!({ "type": "login", "v": 1, "device": device.id }),
        b"",
    );
    let challenge = ws.hear()["challenge"].as_str().unwrap().to_owned();
    ws.say(&json!({ "type": "auth", "signature": device.sign("https://other.example.ts.net", &challenge) }), b"");
    assert_eq!(ws.hear()["reason"], "device");

    // A challenge is good for the connection it was asked on, once.
    let mut ws = sink.phone();
    ws.say(
        &json!({ "type": "login", "v": 1, "device": device.id }),
        b"",
    );
    let fresh = ws.hear()["challenge"].as_str().unwrap().to_owned();
    assert_ne!(fresh, challenge);
    ws.say(&json!({ "type": "auth", "signature": device.sign(&format!("https://{NAME}"), &challenge) }), b"");
    assert_eq!(ws.hear()["reason"], "device");

    for id in ["f".repeat(32), "not-an-id".to_owned(), "A".repeat(32)] {
        let mut ws = sink.phone();
        ws.say(&json!({ "type": "login", "v": 1, "device": id }), b"");
        let said = ws.hear();
        // An id of the right form gets a challenge like any other (the answer does not tell which ids exist).
        if said["type"] == "challenge" {
            ws.say(
                &json!({ "type": "auth", "signature": "0".repeat(128) }),
                b"",
            );
            assert_eq!(ws.hear()["reason"], "device");
        } else {
            assert_eq!(said["reason"], "device");
        }
    }
    // Media before the login is not looked at.
    let mut ws = sink.phone();
    ws.say(&json!({ "type": "segment", "sb": 0, "seq": 0 }), &[0u8; 64]);
    assert_eq!(ws.hear()["reason"], "protocol");
    assert_eq!(sink.captures(), Vec::<String>::new());
}

#[test]
fn revoking_a_device_ends_its_connection_and_its_key() {
    let sink = Sink::tailnet("revoke");
    let keep = Device::pair(&sink, 1);
    let lost = Device::pair(&sink, 2);
    let (said, mut open) = lost.login(&sink);
    assert_eq!(said["type"], "ok");
    let (_, mut kept_open) = keep.login(&sink);

    assert_eq!(
        sink.devices(&["revoke", &"0".repeat(32)], TOKEN).0,
        2,
        "no such device"
    );
    assert_eq!(
        sink.devices(&["revoke", &lost.id], "wrong-token").0,
        1,
        "the command line needs the token"
    );
    assert_eq!(
        lost.login(&sink).0["type"],
        "ok",
        "still paired after a refused revocation"
    );

    let (status, text) = sink.devices(&["revoke", &lost.id], TOKEN);
    assert_eq!(status, 0, "{text}");
    assert!(
        open.closed(),
        "the open connection of the revoked device ends"
    );
    assert_eq!(lost.login(&sink).0["reason"], "device");
    // The other device is untouched.
    kept_open.say(
        &json!({ "type": "hello", "session": "k", "url": "u", "title": "t" }),
        b"",
    );
    wait_for("the other device still captures", || {
        !sink.captures().is_empty()
    });
    assert_eq!(keep.login(&sink).0["type"], "ok");
    let (_, shown) = sink.devices(&[], TOKEN);
    assert!(
        shown.contains(&keep.id) && !shown.contains(&lost.id),
        "{shown}"
    );
}

#[test]
fn a_large_message_before_the_login_ends_the_connection() {
    let mut sink = Sink::tailnet("large");
    let mut ws = sink.phone();
    ws.say(
        &json!({ "type": "login", "v": 1, "device": "a".repeat(32) }),
        &vec![0u8; 1 << 20],
    );
    assert!(ws.closed());
    assert!(sink.alive());
}

#[test]
fn the_pairing_link_opens_a_page_without_a_script() {
    let sink = Sink::tailnet("page");
    let get = |host: &str, path: &str| {
        let mut s = TcpStream::connect(("127.0.0.1", sink.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(
            s,
            "GET {path} HTTP/1.1\r\nHost: {host}\r\nX-Forwarded-For: 100.64.0.7\r\n\r\n"
        )
        .unwrap();
        let mut text = String::new();
        let _ = s.read_to_string(&mut text);
        text
    };
    let page = get(NAME, "/");
    assert!(page.starts_with("HTTP/1.1 200 OK"), "{page}");
    assert!(
        page.contains("Content-Security-Policy: default-src 'none'"),
        "{page}"
    );
    assert!(
        page.contains("Cache-Control: no-store") && !page.contains("<script"),
        "{page}"
    );
    assert!(get(NAME, "/captures").starts_with("HTTP/1.1 404"));
    // Only for the tailnet name: this machine's own browser gets no page from the sink.
    assert!(!get(&format!("127.0.0.1:{}", sink.port), "/").contains("streamtap"));
}

/// With `--tailnet` the sink looks at the first bytes of every connection (the pairing page). Connections that
/// send nothing must not hold the places there: beyond four waiting ones the oldest goes at once, long before the
/// time limit, and a real client still gets in.
#[test]
fn silent_connections_do_not_hold_the_places() {
    let sink = Sink::tailnet("silent");
    let silent: Vec<TcpStream> = (0..8)
        .map(|_| TcpStream::connect(("127.0.0.1", sink.port)).expect("connect"))
        .collect();
    let mut oldest = &silent[0];
    oldest
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let started = std::time::Instant::now();
    let mut byte = [0u8; 1];
    let got = oldest.read(&mut byte);
    let timed_out = matches!(
        &got,
        Err(e) if matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock)
    );
    assert!(
        !timed_out && started.elapsed() < Duration::from_secs(4),
        "the oldest silent connection is closed by the newer ones, not by the time limit ({got:?})"
    );
    let device = Device::pair(&sink, 8);
    assert_eq!(device.login(&sink).0["type"], "ok");
}

/// With `--tailnet` the look at a connection's first bytes comes before the handshake. Both count against the
/// one deadline of `hello`: a client that trickles its request head for most of the limit and then finishes the
/// handshake has only the rest of it left, not a second limit.
#[test]
fn a_trickled_head_gets_no_second_deadline() {
    const LIMIT: Duration = Duration::from_secs(3);
    let sink = Sink::start(
        "onedeadline",
        &["--tailnet", NAME, "--hello-timeout-secs", "3"],
    );
    let mut s = TcpStream::connect(("127.0.0.1", sink.port)).expect("connect");
    let started = Instant::now();
    let head = format!(
        "GET / HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nOrigin: {ORIGIN}\r\n\r\n",
        sink.port
    );
    let (first, rest) = head.split_at(head.len() / 2);
    s.write_all(first.as_bytes()).expect("half the head");
    // Two thirds of the limit pass before the head is complete.
    std::thread::sleep(Duration::from_secs(2));
    s.write_all(rest.as_bytes()).expect("the rest of the head");
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut answer = [0u8; 12];
    s.read_exact(&mut answer).expect("the handshake's answer");
    assert_eq!(&answer, b"HTTP/1.1 101", "the handshake itself is in time");
    // No hello follows. The sink ends the connection when the one limit is over, 3 s after the connect; with
    // a limit of its own for the handshake it would stay open until 5 s.
    let mut sink_closed = false;
    let mut buf = [0u8; 256];
    while started.elapsed() < LIMIT + Duration::from_millis(1200) {
        s.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        match s.read(&mut buf) {
            Ok(0) => sink_closed = true,
            Ok(_) => continue,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                continue
            }
            Err(_) => sink_closed = true,
        }
        break;
    }
    assert!(
        sink_closed,
        "still open {:?} after the connect: the handshake started a limit of its own",
        started.elapsed()
    );
    assert!(
        started.elapsed() >= Duration::from_millis(2500),
        "closed before the limit"
    );
    assert_eq!(sink.captures(), Vec::<String>::new());
}

#[test]
fn a_broken_list_of_devices_stops_the_start() {
    let dir = std::env::temp_dir().join(format!(
        "streamtap-tailnet-test-{}-broken",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let list = dir.join("devices.json");
    std::fs::write(&list, "{ not a list").unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_streamtap-sink"))
            .args(["serve", "--token", TOKEN, "--port", "0", "--out"])
            .arg(&dir)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run the sink")
            .code()
    };
    assert_eq!(
        run(&["--tailnet", NAME, "--devices", list.to_str().unwrap()]),
        Some(1)
    );
    // A name that is not a plain host name, and the tailnet flags without the tailnet.
    for name in [
        "https://pc.example.ts.net",
        "pc.example.ts.net:443",
        "PC.example.ts.net",
        "pc",
        "0.0.0.0.",
    ] {
        assert_eq!(run(&["--tailnet", name]), Some(2), "--tailnet {name}");
    }
    assert_eq!(run(&["--devices", list.to_str().unwrap()]), Some(2));
    let _ = std::fs::remove_dir_all(&dir);
}

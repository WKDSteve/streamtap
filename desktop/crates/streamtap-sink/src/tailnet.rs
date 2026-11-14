//! Phones over the tailnet (`serve --tailnet <name>`), and the sink's own command line for them (`devices`).
//!
//! The sink still binds `127.0.0.1` only (`MOB-01`). `tailscale serve` on the same machine ends TLS for the machine's
//! name in the tailnet and forwards to that port, so a connection of a phone arrives on loopback like any other, told
//! apart by its `Host` header (the tailnet name, exactly) and by the headers a forwarding proxy adds. Such a
//! connection never gets in with the shared token: it is a paired device or nothing.
//!
//! Pairing (`MOB-04`): `streamtap-sink devices pair` asks the running sink for a one-time code (128 bit, 60 s, one
//! use) and shows it as a link and a QR code. The phone sends the code with a name and the public half of an ECDSA
//! P-256 key it keeps non-extractable; the sink keeps that in its list of devices (`devices.json`, nothing secret in
//! it). From then on the phone opens a connection with `login`, signs the sink's challenge together with the origin
//! it speaks to, and only then says `hello`. `devices revoke <id>` forgets one device and ends its open connections.
//! The list, the code and the check of the signature are appkit's (`MOB-11`).
//!
//! What the sink says to a phone are WebSocket text messages (JSON); what a phone says keeps the sink's framing.

use super::*;
use appkit_core::authn::{Clock, PairingStore, RateLimiter, SystemClock};
use appkit_hub::devices::{check_pairing, DeviceStore, Level, PairError};
use tungstenite::handshake::server::{ErrorResponse, Request, Response};

/// The version of the device messages (`pair`, `login`). Every answer names it, a refusal also the lowest the sink
/// takes (`min`); a phone that names another version is refused with the reason `version` (`MOB-10`).
pub(crate) const PROTOCOL: u32 = 1;
/// How long a pairing code lives.
pub(crate) const PAIR_TTL: Duration = Duration::from_secs(60);
/// Pairing attempts the sink looks at per minute, whoever makes them.
const MAX_PAIRS_PER_MINUTE: usize = 10;
/// The largest message of a phone before it is logged in (a key is 130 characters).
pub(crate) const MAX_GATE_MESSAGE: usize = 4096;
/// The largest request head read to answer a plain `GET` of the pairing link.
const MAX_HEAD: usize = 8192;
/// Default of `--tailnet-session-hours`: the longest one connection of a phone lasts.
pub(crate) const DEFAULT_TAILNET_SESSION_HOURS: u64 = 12;
/// Characters of a device id (16 bytes as hex).
const DEVICE_ID_LEN: usize = 32;

/// The page a phone's browser gets when it opens the pairing link: the code is in the fragment, which never reaches
/// the sink; the extension reads it from the address and asks in its popup.
const PAIR_PAGE: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>streamtap</title></head>\
<body><h1>streamtap</h1><p>This is the streamtap sink of your PC.</p>\
<p>To pair this phone, open the streamtap add-on now (browser menu, Extensions, streamtap) and confirm the pairing \
there. The link is good for one minute and one use.</p></body></html>";

/// What `serve` was told about the tailnet.
#[derive(Debug, Clone)]
pub(crate) struct TailnetConfig {
    pub(crate) name: String,
    pub(crate) devices: PathBuf,
    pub(crate) max_session: Duration,
}

/// The name of this machine in the tailnet as `--tailnet` takes it: a plain host name (lower case, with a dot, no
/// scheme, no port), as `tailscale status` and `MagicDNS` show it (`pc.example.ts.net`, with Headscale
/// `pc.<base_domain>`).
pub(crate) fn tailnet_name(value: &str) -> Result<String, SinkError> {
    let label_ok = |l: &str| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
    };
    if value.len() <= 253 && value.contains('.') && value.split('.').all(label_ok) {
        return Ok(value.to_owned());
    }
    Err(SinkError::Usage(
        "--tailnet must be the name of this machine in the tailnet: lower case, no https://, no port (pc.example.ts.net)"
            .into(),
    ))
}

/// The way a connection came.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    /// From this machine: the extension of a desktop browser (exact origin, shared token) or the sink's command line.
    Local,
    /// Through Tailscale Serve: a paired device, or one that pairs.
    Remote,
}

/// A connection of a logged-in device; dropping it takes the connection off the list `revoke` closes.
pub(crate) struct Live {
    tailnet: Arc<Tailnet>,
    id: u64,
    pub(crate) device: String,
}

impl Drop for Live {
    fn drop(&mut self) {
        lock(&self.tailnet.live).retain(|(id, _, _)| *id != self.id);
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The tailnet side of a running sink.
pub(crate) struct Tailnet {
    pub(crate) name: String,
    /// What a device signs with its challenge: the origin it speaks to.
    origin: String,
    devices: DeviceStore,
    codes: PairingStore,
    pair_limit: RateLimiter,
    /// The open connections of logged-in devices, to end them on a revocation.
    live: Mutex<Vec<(u64, String, TcpStream)>>,
    next_live: AtomicU64,
    pub(crate) max_session: Duration,
}

impl Tailnet {
    /// Opens the list of devices; a list that cannot be read stops the start (the sink does not forget whom it trusts).
    pub(crate) fn open(config: &TailnetConfig) -> Result<Arc<Self>, SinkError> {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
        let devices = DeviceStore::open(&config.devices, Arc::clone(&clock)).map_err(|e| {
            SinkError::io(
                "cannot open the list of devices",
                Some(config.devices.clone()),
                e,
            )
        })?;
        Ok(Arc::new(Self {
            origin: format!("https://{}", config.name),
            name: config.name.clone(),
            devices,
            codes: PairingStore::new(Arc::clone(&clock), PAIR_TTL),
            pair_limit: RateLimiter::new(clock, MAX_PAIRS_PER_MINUTE, Duration::from_secs(60)),
            live: Mutex::new(Vec::new()),
            next_live: AtomicU64::new(0),
            max_session: config.max_session,
        }))
    }

    pub(crate) fn paired(&self) -> usize {
        self.devices.list().len()
    }

    /// Forgets a device and ends its open connections. `Ok(false)`: no such device.
    fn revoke(&self, device: &str) -> io::Result<bool> {
        if !self.devices.revoke(device)? {
            return Ok(false);
        }
        for (_, d, stream) in lock(&self.live).iter() {
            if d == device {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        }
        Ok(true)
    }

    /// The door of a remote connection: one `pair`, or `login` and `auth`. `Some` once a device is logged in; `None`
    /// when the connection is over (paired, refused, broken). Every refusal is said to the phone before the close, so
    /// it can show what happened (`MOB-09`).
    pub(crate) fn gate(
        self: &Arc<Self>,
        ws: &mut tungstenite::WebSocket<Timed>,
        peer: std::net::SocketAddr,
    ) -> Option<Live> {
        let first = next_binary(ws, peer)?;
        match split_message(&first) {
            Ok((Header::Pair { v, code, name, key }, _)) => {
                let said = if v == PROTOCOL {
                    self.pair(&code, &name, &key, peer)
                } else {
                    refusal("version")
                };
                tell(ws, &said);
                None
            }
            Ok((Header::Login { v, device }, _)) => {
                if v != PROTOCOL {
                    tell(ws, &refusal("version"));
                    return None;
                }
                self.login(ws, peer, device)
            }
            _ => {
                eprintln!("{peer}: a tailnet connection must pair or log in first, closing");
                tell(ws, &refusal("protocol"));
                None
            }
        }
    }

    fn pair(
        &self,
        code: &str,
        name: &str,
        key: &str,
        peer: std::net::SocketAddr,
    ) -> serde_json::Value {
        if !self.pair_limit.allow() {
            eprintln!("{peer}: too many pairing attempts, refused");
            return refusal("busy");
        }
        // Before the code is spent: a phone that sends a bad key keeps its code for a second try.
        let reason = |e: &PairError| match e {
            PairError::BadName => "name",
            PairError::BadKey => "key",
            PairError::Known => "known",
            PairError::Full => "full",
            _ => "failed",
        };
        if let Err(e) = check_pairing(name, key, None) {
            return refusal(reason(&e));
        }
        if !self.codes.redeem(code) {
            eprintln!("{peer}: pairing refused: the code is wrong, used or older than a minute");
            return refusal("code");
        }
        match self.devices.pair(name, key, None, Level::Create) {
            Ok(device) => {
                println!("{peer}: paired device {device} ({})", printable(name, 60));
                serde_json::json!({ "type": "paired", "v": PROTOCOL, "device": device, "hub": self.devices.hub_id() })
            }
            Err(e) => {
                eprintln!("{peer}: pairing refused ({})", reason(&e));
                refusal(reason(&e))
            }
        }
    }

    fn login(
        self: &Arc<Self>,
        ws: &mut tungstenite::WebSocket<Timed>,
        peer: std::net::SocketAddr,
        device: String,
    ) -> Option<Live> {
        // An id of another form is never one of ours; it is not kept for a challenge either.
        let formed = device.len() == DEVICE_ID_LEN
            && device
                .bytes()
                .all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'));
        let challenge = formed
            .then(|| self.devices.challenge(&device).ok())
            .flatten();
        let Some(challenge) = challenge else {
            tell(ws, &refusal("device"));
            return None;
        };
        // The id of this sink goes first: the phone signs only for the sink it paired with (`MSEC-04`).
        let asked = serde_json::json!({ "type": "challenge", "v": PROTOCOL, "hub": self.devices.hub_id(), "challenge": challenge });
        if ws
            .send(tungstenite::Message::text(asked.to_string()))
            .is_err()
        {
            return None;
        }
        let answer = next_binary(ws, peer)?;
        let Ok((Header::Auth { signature }, _)) = split_message(&answer) else {
            tell(ws, &refusal("protocol"));
            return None;
        };
        if !self
            .devices
            .login(&device, &challenge, &signature, &self.origin)
        {
            eprintln!("{peer}: login refused: not a paired device, or a wrong signature");
            tell(ws, &refusal("device"));
            return None;
        }
        let Ok(stream) = ws.get_ref().inner.try_clone() else {
            tell(ws, &refusal("failed"));
            return None;
        };
        let id = self.next_live.fetch_add(1, Ordering::SeqCst);
        lock(&self.live).push((id, device.clone(), stream));
        let live = Live {
            tailnet: Arc::clone(self),
            id,
            device,
        };
        // Revoked between the signature and the list: the list is what `revoke` closes, so look once more.
        if self.devices.level_of(&live.device).is_none() {
            tell(ws, &refusal("device"));
            return None;
        }
        let ok = serde_json::json!({ "type": "ok", "v": PROTOCOL });
        if ws.send(tungstenite::Message::text(ok.to_string())).is_err() {
            return None;
        }
        Some(live)
    }
}

fn refusal(reason: &str) -> serde_json::Value {
    serde_json::json!({ "type": "refused", "v": PROTOCOL, "min": PROTOCOL, "reason": reason })
}

/// Says one last thing and closes.
pub(crate) fn tell(ws: &mut tungstenite::WebSocket<Timed>, value: &serde_json::Value) {
    let _ = ws.send(tungstenite::Message::text(value.to_string()));
    let _ = ws.close(None);
    let _ = ws.flush();
}

pub(crate) fn forbidden(why: &str) -> ErrorResponse {
    let mut err = ErrorResponse::new(Some(why.to_owned()));
    *err.status_mut() = tungstenite::http::StatusCode::FORBIDDEN;
    err
}

/// Which way a handshake came. A request a proxy forwarded (Tailscale Serve adds `X-Forwarded-*` and, for a user of
/// the tailnet, `Tailscale-*` headers) is remote whatever `Host` it claims, and is only taken for the name
/// `--tailnet` gave: without the flag nothing forwarded gets to the token check at all.
///
/// Everything else is local and must have asked for this sink by a loopback name and the port it is bound to
/// (`is_loopback_host`, SRV-03): a page whose own host name was pointed at 127.0.0.1 (DNS rebinding) names
/// that host, and stops here whatever `Origin` it sends.
pub(crate) fn classify(
    tailnet: Option<&Tailnet>,
    port: u16,
    req: &Request,
) -> Result<Class, &'static str> {
    let host = req
        .headers()
        .get("host")
        .map(tungstenite::http::HeaderValue::as_bytes);
    if tailnet.is_some_and(|t| host == Some(t.name.as_bytes())) {
        return Ok(Class::Remote);
    }
    let forwarded = req.headers().keys().any(|k| {
        let k = k.as_str();
        k.starts_with("x-forwarded-") || k.starts_with("tailscale-") || k == "forwarded"
    });
    if forwarded {
        return Err("forwarded for a host this sink does not answer");
    }
    if !host.is_some_and(|h| is_loopback_host(h, port)) {
        return Err("asked for a host this sink does not answer");
    }
    Ok(Class::Local)
}

/// The `Host` of a request to this sink over loopback: `127.0.0.1`, `localhost` or `[::1]` with the bound port.
/// The port may only be left out where a client leaves it out, on port 80. Host names compare without case;
/// nothing is trimmed or normalised beyond that (no trailing dot, no user info, no other spelling of the
/// address).
pub(crate) fn is_loopback_host(host: &[u8], port: u16) -> bool {
    const NAMES: [&str; 3] = ["127.0.0.1", "localhost", "[::1]"];
    let Ok(host) = std::str::from_utf8(host) else {
        return false;
    };
    let name = match host.rsplit_once(':') {
        // `[::1]` without a port holds colons of its own.
        Some((name, p)) if !p.ends_with(']') => {
            if p != port.to_string() {
                return false;
            }
            name
        }
        _ if port == 80 => host,
        _ => return false,
    };
    NAMES.iter().any(|n| name.eq_ignore_ascii_case(n))
}

/// The origin of a remote handshake must be an extension's (`moz-extension://<id>`, `chrome-extension://<id>`): a
/// web page open on the phone has an `http(s)` origin and stops here. Which extension it is does not matter (Firefox
/// gives every installation its own id); the device key decides.
// The error type is fixed by tungstenite's handshake `Callback` trait; it cannot be boxed.
#[allow(clippy::result_large_err)]
pub(crate) fn check_remote_origin(
    req: &Request,
    resp: Response,
) -> Result<Response, ErrorResponse> {
    let origin = req
        .headers()
        .get("origin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let id = origin
        .strip_prefix("moz-extension://")
        .or_else(|| origin.strip_prefix("chrome-extension://"));
    let ok = id.is_some_and(|id| {
        (1..=64).contains(&id.len())
            && id
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
    });
    if ok {
        return Ok(resp);
    }
    eprintln!("tailnet origin rejected: {}", printable(origin, 120));
    Err(forbidden("origin not allowed"))
}

/// What the look at a connection's first bytes found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Plain {
    /// A plain `GET` for the tailnet name: answered, the connection is done.
    Answered,
    /// Nothing usable came (closed, silent until the limit, pushed out by newer connections): the connection is done.
    Gone,
    /// Anything else, left unread for the WebSocket handshake.
    Handshake,
}

/// Answers a plain `GET` for the tailnet name (the phone's browser opening the pairing link) with a small page.
/// Anything else (a WebSocket handshake, another host) is left unread for the handshake. A connection that sends no
/// request head is held to the same rules as one that waits for `hello`: until `until` at most (the deadline of
/// the connection's `hello`, which the handshake after this shares), and it gives way as soon as newer
/// connections push it out, so silent connections cannot hold the places here either.
pub(crate) fn answer_plain_get(
    stream: &mut TcpStream,
    tailnet: &Tailnet,
    until: Instant,
    pushed_out: &AtomicBool,
) -> Plain {
    let mut buf = [0u8; MAX_HEAD];
    let head = loop {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() || pushed_out.load(Ordering::SeqCst) {
            return Plain::Gone;
        }
        if stream
            .set_read_timeout(Some(left.min(WAITING_POLL)))
            .is_err()
        {
            return Plain::Gone;
        }
        match stream.peek(&mut buf) {
            Ok(0) => return Plain::Gone,
            Ok(n) => {
                if let Some(end) = buf[..n].windows(4).position(|w| w == b"\r\n\r\n") {
                    break &buf[..end];
                }
                // A head larger than any browser sends for this page: the handshake refuses it.
                if n == buf.len() {
                    return Plain::Handshake;
                }
                // `peek` returns at once while the same bytes wait; give the rest time to arrive.
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) => {}
            Err(_) => return Plain::Gone,
        }
    };
    let Ok(head) = std::str::from_utf8(head) else {
        return Plain::Handshake;
    };
    let mut lines = head.split("\r\n");
    let mut request = lines.next().unwrap_or("").split(' ');
    let (method, path) = (request.next().unwrap_or(""), request.next().unwrap_or(""));
    let mut host = None;
    let mut upgrade = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("host") {
            host = Some(value.trim());
        } else if name.eq_ignore_ascii_case("upgrade") {
            upgrade = true;
        }
    }
    if upgrade || method != "GET" || host != Some(tailnet.name.as_str()) {
        return Plain::Handshake;
    }
    let (status, body) = if path == "/" {
        ("200 OK", PAIR_PAGE)
    } else {
        ("404 Not Found", "")
    };
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\n\
         Content-Security-Policy: default-src 'none'; frame-ancestors 'none'\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.flush();
    Plain::Answered
}

/// What the running sink answers its own command line (`devices`), as JSON.
pub(crate) fn control(tailnet: Option<&Arc<Tailnet>>, op: &str, device: &str) -> serde_json::Value {
    let fail = |error: &str| serde_json::json!({ "ok": false, "error": error });
    let Some(t) = tailnet else {
        return fail("this sink was started without --tailnet: it has no devices");
    };
    match op {
        "pair" => match t.codes.mint() {
            Ok(code) => serde_json::json!({
                "ok": true, "tailnet": t.name, "code": code.to_hex(), "ttl": PAIR_TTL.as_secs(),
            }),
            Err(_) => fail("the system has no random source"),
        },
        "devices" => {
            let live = lock(&t.live);
            let devices: Vec<serde_json::Value> = t
                .devices
                .list()
                .into_iter()
                .map(|d| {
                    let open = live.iter().filter(|(_, id, _)| *id == d.id).count();
                    serde_json::json!({
                        "id": d.id, "name": d.name, "level": d.level.word(), "paired": d.paired, "last_used": d.last_used, "open": open,
                    })
                })
                .collect();
            serde_json::json!({ "ok": true, "tailnet": t.name, "devices": devices })
        }
        "revoke" => match t.revoke(device) {
            Ok(revoked) => serde_json::json!({ "ok": true, "revoked": revoked }),
            Err(e) => fail(&format!("the list of devices could not be written: {e}")),
        },
        _ => fail("unknown command"),
    }
}

// ---------------------------------------------------------------------------
// The command line: `streamtap-sink devices [pair | revoke <id>]`
// ---------------------------------------------------------------------------

/// How long the command line waits for the running sink.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);

/// Asks the running sink on `port` and returns its answer.
fn ask_sink(
    port: u16,
    origin: &str,
    request: &serde_json::Value,
) -> Result<serde_json::Value, SinkError> {
    use tungstenite::client::IntoClientRequest;
    let proto = |what: String| SinkError::Protocol(what);
    let stream = TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), CONTROL_TIMEOUT).map_err(|e| {
        SinkError::io(
            format!("no sink answers on 127.0.0.1:{port} (start `streamtap-sink serve --tailnet <name>` first)"),
            None,
            e,
        )
    })?;
    for set in [TcpStream::set_read_timeout, TcpStream::set_write_timeout] {
        set(&stream, Some(CONTROL_TIMEOUT)).map_err(|e| SinkError::io("set a timeout", None, e))?;
    }
    let mut req = format!("ws://127.0.0.1:{port}/")
        .into_client_request()
        .map_err(|e| proto(e.to_string()))?;
    let origin = tungstenite::http::HeaderValue::from_str(origin)
        .map_err(|_| SinkError::Usage("--allow-origin is not a header value".into()))?;
    req.headers_mut().insert("origin", origin);
    let (mut ws, _) = tungstenite::client(req, stream).map_err(|e| {
        proto(format!(
            "the sink on 127.0.0.1:{port} refused the connection ({e}); name the --allow-origin it was started with"
        ))
    })?;
    let header = request.to_string().into_bytes();
    let mut frame = u32::try_from(header.len())
        .map_err(|_| proto("request too large".into()))?
        .to_le_bytes()
        .to_vec();
    frame.extend_from_slice(&header);
    ws.send(tungstenite::Message::binary(frame))
        .map_err(|e| proto(e.to_string()))?;
    loop {
        match ws.read() {
            Ok(tungstenite::Message::Text(t)) => {
                return serde_json::from_str(t.as_str()).map_err(|e| proto(e.to_string()))
            }
            Ok(tungstenite::Message::Close(_)) | Err(_) => {
                return Err(proto(
                    "the sink closed without an answer: a wrong token, or a sink older than this command".into(),
                ))
            }
            Ok(_) => {}
        }
    }
}

/// Seconds since 1970 as a UTC date and time, for the list of devices.
fn utc(secs: u64) -> String {
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm), in unsigned arithmetic.
    let z = days + 719_468;
    let (era, doe) = (z / 146_097, z % 146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let (d, m) = (
        doy - (153 * mp + 2) / 5 + 1,
        if mp < 10 { mp + 3 } else { mp - 9 },
    );
    let y = yoe + era * 400 + u64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        rest / 3600,
        rest % 3600 / 60
    )
}

/// The pairing link of a phone: the code is in the fragment (`MSEC-02`), which a browser sends to no server.
pub(crate) fn pair_link(tailnet: &str, code: &str) -> String {
    format!("https://{tailnet}/#streamtap-pair=1&code={code}")
}

/// The modules of the QR code of `text`, one string of `1` (dark) and `0` per row, without a quiet zone; empty when
/// the text is too long for a code.
fn qr_rows(text: &str) -> Vec<String> {
    let Ok(code) = qrcode::QrCode::with_error_correction_level(text.as_bytes(), qrcode::EcLevel::M)
    else {
        return Vec::new();
    };
    code.to_colors()
        .chunks(code.width())
        .map(|row| {
            row.iter()
                .map(|c| if *c == qrcode::Color::Dark { '1' } else { '0' })
                .collect()
        })
        .collect()
}

/// `--json`: the sink's answer as one line of JSON on stdout, for a program (the desktop app) in place of the
/// text for a person. A pairing also carries its link and the modules of the link's QR code (`qr`).
fn print_json(op: &str, device: &str, mut answer: serde_json::Value) -> Result<(), SinkError> {
    if op == "pair" {
        let link = pair_link(
            answer["tailnet"].as_str().unwrap_or(""),
            answer["code"].as_str().unwrap_or(""),
        );
        answer["qr"] = qr_rows(&link).into();
        answer["link"] = link.into();
    }
    if op == "revoke" && answer["revoked"] != true {
        return Err(SinkError::Usage(format!(
            "no paired device has the id {device}"
        )));
    }
    println!("{answer}");
    Ok(())
}

pub(crate) fn run_devices(args: &[String]) -> Result<(), SinkError> {
    let usage = || {
        SinkError::Usage(
            "usage:\n  streamtap-sink devices (--token-stdin | --token <token>) [--port <port>]            the paired phones\n  streamtap-sink devices pair (--token-stdin | --token <token>) [--port <port>]       a one-time pairing link\n  streamtap-sink devices revoke <id> (--token-stdin | --token <token>) [--port <port>]\n  --json prints the sink's answer as one line of JSON (a pairing with its link and the modules of its QR code)"
                .into(),
        )
    };
    let (op, device, rest) = match args.first().map(String::as_str) {
        Some("pair") => ("pair", "", &args[1..]),
        Some("revoke") => match args.get(1) {
            Some(id) if !id.starts_with("--") => ("revoke", id.as_str(), &args[2..]),
            _ => return Err(usage()),
        },
        Some(a) if !a.starts_with("--") => return Err(usage()),
        _ => ("devices", "", args),
    };
    let port = match flag(rest, "--port")?.map(str::parse::<u16>) {
        None => DEFAULT_PORT,
        Some(Ok(p)) if p != 0 => p,
        Some(_) => return Err(SinkError::Usage("--port must be a number".into())),
    };
    let origin = flag(rest, "--allow-origin")?.unwrap_or(DEFAULT_ORIGIN);
    let token = serve_token(rest, io::stdin().lock())?;
    let token = String::from_utf8_lossy(&token.0).into_owned();
    let answer = ask_sink(
        port,
        origin,
        &serde_json::json!({ "type": "control", "token": token, "op": op, "device": device }),
    )?;
    if answer["ok"] != true {
        return Err(SinkError::Protocol(
            answer["error"]
                .as_str()
                .unwrap_or("the sink refused")
                .to_owned(),
        ));
    }
    if rest.iter().any(|a| a == "--json") {
        return print_json(op, device, answer);
    }
    let name = answer["tailnet"].as_str().unwrap_or("");
    match op {
        "pair" => {
            let link = pair_link(name, answer["code"].as_str().unwrap_or(""));
            println!(
                "Pair a phone (good for {} seconds, one use):\n",
                answer["ttl"]
            );
            if let Ok(qr) = appkit_hub::phone::qr_text(&link) {
                println!("{qr}");
            }
            println!("{link}\n");
            println!("Scan the code with the phone's camera and open the link in the browser that has the streamtap");
            println!("add-on, then confirm in the add-on's popup. Or type into the popup (Target, pair this phone):");
            println!(
                "  PC:   {name}\n  code: {}",
                answer["code"].as_str().unwrap_or("")
            );
        }
        "revoke" => {
            if answer["revoked"] == true {
                println!("revoked {device}: its connections are closed, it has to pair again");
            } else {
                return Err(SinkError::Usage(format!(
                    "no paired device has the id {device}"
                )));
            }
        }
        _ => {
            let devices = answer["devices"].as_array().map_or(&[][..], Vec::as_slice);
            println!("{} paired device(s) of https://{name}", devices.len());
            for d in devices {
                let at = |v: &serde_json::Value| v.as_u64().map_or_else(|| "never".to_owned(), utc);
                println!(
                    "  {}  {}  level {}  paired {}  last used {}  open connections {}",
                    d["id"].as_str().unwrap_or("?"),
                    printable(d["name"].as_str().unwrap_or(""), 60),
                    d["level"].as_str().unwrap_or("?"),
                    at(&d["paired"]),
                    at(&d["last_used"]),
                    d["open"],
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tailnet_name_is_a_plain_lower_case_host() {
        for ok in [
            "pc.example.ts.net",
            "my-pc.tail1234.ts.net",
            "pc.hs.example.org",
        ] {
            assert_eq!(tailnet_name(ok).expect(ok), ok);
        }
        for bad in [
            "",
            "pc",
            "PC.example.ts.net",
            "https://pc.example.ts.net",
            "pc.example.ts.net:443",
            "pc.example.ts.net/",
            "pc..ts.net",
            "-pc.ts.net",
            "pc.ts.net.",
            "pc .ts.net",
        ] {
            assert_eq!(tailnet_name(bad).unwrap_err().exit_code(), 2, "{bad:?}");
        }
    }

    #[test]
    fn dates_of_the_device_list_are_utc() {
        assert_eq!(utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(utc(951_782_400), "2000-02-29 00:00 UTC");
        assert_eq!(utc(1_794_571_140), "2026-11-13 11:59 UTC");
    }

    #[test]
    fn the_pairing_link_keeps_the_code_in_the_fragment() {
        let link = pair_link("pc.example.ts.net", "00ff");
        assert_eq!(
            link,
            "https://pc.example.ts.net/#streamtap-pair=1&code=00ff"
        );
        assert!(!link.split('#').next().unwrap_or("").contains("00ff"));
    }

    /// A device's connection ends at its time limit even while it keeps sending (`MSEC-08`).
    #[test]
    fn a_connection_past_its_time_limit_reads_nothing_more() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let mut client = TcpStream::connect(listener.local_addr().expect("addr")).expect("connect");
        let (server, _) = listener.accept().expect("accept");
        let limits = Limits {
            hello: Duration::from_secs(10),
            idle: Duration::from_secs(3600),
            max_connections: 1,
        };
        let mut timed = Timed::new(
            server,
            Instant::now() + limits.hello,
            limits,
            Arc::new(AtomicBool::new(false)),
        );
        timed.worked();
        client.write_all(b"x").expect("write");
        let mut buf = [0u8; 1];
        timed.end_at = Some(Instant::now() + Duration::from_secs(60));
        assert_eq!(timed.read(&mut buf).expect("within the limit"), 1);
        client.write_all(b"y").expect("write");
        timed.end_at = Some(Instant::now());
        assert_eq!(
            timed.read(&mut buf).expect_err("past the limit").kind(),
            io::ErrorKind::TimedOut
        );
    }
}

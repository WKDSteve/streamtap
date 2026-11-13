//! `serve`: the listener, the checks of a connection and its read loop.

use super::*;

/// Longest token line `--token-stdin` reads (without its line break).
pub(crate) const MAX_TOKEN_LINE: usize = 4096;

/// The shared token. It has no `PartialEq` (compare with [`Secret::matches`],
/// in constant time) and its `Debug` does not print it (SRV-11).
#[derive(Clone)]
pub(crate) struct Secret(pub(crate) Vec<u8>);

impl Secret {
    /// Compares `other` with the token in time that depends on the lengths only.
    pub(crate) fn matches(&self, other: &[u8]) -> bool {
        constant_time_eq(other, &self.0)
    }

    #[cfg(test)]
    pub(crate) fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(hidden)")
    }
}

/// `serve`'s settings, checked before anything is created or bound.
#[derive(Debug)]
pub(crate) struct ServeConfig {
    pub(crate) token: Secret,
    pub(crate) port: u16,
    pub(crate) out: PathBuf,
    pub(crate) origin: String,
    pub(crate) per_session: u64,
    pub(crate) total_max: u64,
    pub(crate) limits: Limits,
    /// `--tailnet`: phones through Tailscale Serve; `None`: local connections only.
    pub(crate) tailnet: Option<TailnetConfig>,
}

/// The time and connection limits of `serve`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    pub(crate) hello: Duration,
    pub(crate) idle: Duration,
    pub(crate) max_connections: usize,
}

/// A client socket with a deadline to the first `hello` and an idle timeout
/// after it. Both are absolute: a client that trickles bytes or sends pings
/// does not start them over, as a plain per-read socket timeout would. Only a
/// message that does work does (`Timed::worked`).
pub(crate) struct Timed {
    pub(crate) inner: TcpStream,
    /// Moment by which `hello` must be in; `None` once it was.
    pub(crate) deadline: Option<Instant>,
    /// Moment the session counts as idle; `None` until `hello`.
    pub(crate) idle_until: Option<Instant>,
    pub(crate) idle: Duration,
    /// Moment the connection ends whatever it does (a device's connection, `--tailnet-session-hours`).
    pub(crate) end_at: Option<Instant>,
    /// Set when a newer connection pushed this one out while it waited for `hello`.
    pub(crate) pushed_out: Arc<AtomicBool>,
}

impl Timed {
    /// The handshake and the first message share one deadline.
    pub(crate) fn new(inner: TcpStream, limits: Limits, pushed_out: Arc<AtomicBool>) -> Self {
        Self {
            inner,
            deadline: Some(Instant::now() + limits.hello),
            idle_until: None,
            idle: limits.idle,
            end_at: None,
            pushed_out,
        }
    }

    /// A message that does work came in: `hello` is over, the idle time starts anew.
    pub(crate) fn worked(&mut self) {
        self.deadline = None;
        self.idle_until = Some(Instant::now() + self.idle);
    }

    pub(crate) fn arm(&self) -> io::Result<()> {
        let until = match (self.deadline.or(self.idle_until), self.end_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let Some(until) = until else {
            return self.inner.set_read_timeout(Some(self.idle));
        };
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        // Waiting for `hello`, the read wakes up to see whether it was pushed out: closing the
        // socket from the accept loop does not end a blocked read on every system.
        let wait = if self.deadline.is_some() {
            left.min(WAITING_POLL)
        } else {
            left
        };
        self.inner.set_read_timeout(Some(wait))
    }
}

impl Read for Timed {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            self.arm()?;
            let waiting = self.deadline.is_some();
            if waiting && self.pushed_out.load(Ordering::SeqCst) {
                return Err(io::ErrorKind::ConnectionAborted.into());
            }
            match self.inner.read(buf) {
                Err(e)
                    if waiting
                        && matches!(
                            e.kind(),
                            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                        ) => {}
                other => return other,
            }
        }
    }
}

impl Write for Timed {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// The connections that have not said `hello` yet, oldest first.
#[derive(Default)]
pub(crate) struct Waiting {
    pub(crate) next: AtomicUsize,
    pub(crate) entries: Mutex<VecDeque<(usize, Arc<AtomicBool>)>>,
}

impl Waiting {
    /// Adds a connection and pushes out the oldest above `MAX_WAITING_FOR_HELLO`: its thread sees
    /// the flag within `WAITING_POLL` and frees its place.
    pub(crate) fn add(self: &Arc<Self>) -> WaitingEntry {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let pushed_out = Arc::new(AtomicBool::new(false));
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.push_back((id, Arc::clone(&pushed_out)));
        while entries.len() > MAX_WAITING_FOR_HELLO {
            if let Some((_, oldest)) = entries.pop_front() {
                oldest.store(true, Ordering::SeqCst);
            }
        }
        WaitingEntry {
            id,
            waiting: Arc::clone(self),
            pushed_out,
        }
    }
}

/// A connection's place among those waiting for `hello`: dropped on `hello` or at its end.
pub(crate) struct WaitingEntry {
    pub(crate) id: usize,
    pub(crate) waiting: Arc<Waiting>,
    pub(crate) pushed_out: Arc<AtomicBool>,
}

impl Drop for WaitingEntry {
    fn drop(&mut self) {
        self.waiting
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(id, _)| *id != self.id);
    }
}

/// Counts a live connection; frees its place when the connection thread ends
/// (or never starts).
pub(crate) struct Slot(pub(crate) Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Reads the token for `--token-stdin`: the first line of `r`, at most
/// `MAX_TOKEN_LINE` bytes, LF or CRLF and a leading UTF-8 BOM removed; a
/// longer line is refused, never cut. A last line without a line
/// break counts when it is not empty. Anything after the first line is ignored.
pub(crate) fn read_token_line(r: impl Read) -> Result<String, SinkError> {
    // Room for a BOM before and a CRLF after the longest token, so a line cut
    // at the limit is always longer than a token once both are removed.
    let limit = u64::try_from(MAX_TOKEN_LINE).map_or(u64::MAX, |n| n + 5);
    let mut line = Vec::new();
    io::BufReader::new(r.take(limit))
        .read_until(b'\n', &mut line)
        .map_err(|e| SinkError::io("cannot read the token from stdin", None, e))?;
    if line.last() == Some(&b'\n') {
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
    }
    // Windows PowerShell 5.1 with `$OutputEncoding` set to UTF-8 puts a BOM
    // in front of what it pipes to a program; it is never part of a token.
    if line.starts_with(b"\xef\xbb\xbf") {
        line.drain(..3);
    }
    if line.len() > MAX_TOKEN_LINE {
        return Err(SinkError::Usage(format!(
            "--token-stdin: the token line is longer than {MAX_TOKEN_LINE} bytes"
        )));
    }
    String::from_utf8(line)
        .map_err(|_| SinkError::Usage("--token-stdin: the token is not UTF-8".into()))
}

/// The token from `--token <value>` or, with `--token-stdin`, from the first
/// line of `stdin` (which is read only then). Exactly one of the two.
pub(crate) fn serve_token(args: &[String], stdin: impl Read) -> Result<Secret, SinkError> {
    let from_stdin = args.iter().any(|a| a == "--token-stdin");
    let token = match (flag(args, "--token")?, from_stdin) {
        (Some(_), true) => {
            return Err(SinkError::Usage(
                "--token and --token-stdin exclude each other".into(),
            ))
        }
        (Some(t), false) => t.to_owned(),
        (None, true) => read_token_line(stdin)?,
        (None, false) => {
            return Err(SinkError::Usage(
                "--token-stdin or --token is required (the extension must send the same value)"
                    .into(),
            ))
        }
    };
    // An empty token (an unset variable in `--token "$VAR"`, a closed
    // stdin) would let any local process in with an empty hello, while the
    // extension never connects without one.
    if token.trim().is_empty() {
        return Err(SinkError::Usage("the token must not be empty".into()));
    }
    Ok(Secret(token.into_bytes()))
}

pub(crate) fn serve_config(args: &[String], stdin: impl Read) -> Result<ServeConfig, SinkError> {
    let port = match flag(args, "--port")?.map(str::parse::<u16>) {
        None => DEFAULT_PORT,
        Some(Ok(p)) => p,
        Some(Err(_)) => return Err(SinkError::Usage("--port must be a number".into())),
    };
    let out = PathBuf::from(flag(args, "--out")?.unwrap_or("captures"));
    let origin = flag(args, "--allow-origin")?
        .unwrap_or(DEFAULT_ORIGIN)
        .to_owned();
    let gib = |name: &str, default: u64| match flag(args, name)?.map(str::parse::<u64>) {
        None => Ok(default << 30),
        Some(Ok(g)) if (1..=4096).contains(&g) => Ok(g << 30),
        Some(_) => Err(SinkError::Usage(format!(
            "{name} must be a whole number from 1 to 4096"
        ))),
    };
    let per_session = gib("--max-session-gib", DEFAULT_MAX_SESSION_GIB)?;
    let total_max = gib("--max-total-gib", DEFAULT_MAX_TOTAL_GIB)?;
    // Last: a bad flag ends the sink before it waits for stdin.
    let secs = |name: &str, default: u64, max: u64| match flag(args, name)?.map(str::parse::<u64>) {
        None => Ok(Duration::from_secs(default)),
        Some(Ok(n)) if (1..=max).contains(&n) => Ok(Duration::from_secs(n)),
        Some(_) => Err(SinkError::Usage(format!(
            "{name} must be a whole number from 1 to {max}"
        ))),
    };
    let hello = secs("--hello-timeout-secs", DEFAULT_HELLO_TIMEOUT_SECS, 600)?;
    let idle = secs("--idle-timeout-secs", DEFAULT_IDLE_TIMEOUT_SECS, 86_400)?;
    let max_connections = match flag(args, "--max-connections")?.map(str::parse::<usize>) {
        None => DEFAULT_MAX_CONNECTIONS,
        Some(Ok(n)) if (1..=1024).contains(&n) => n,
        Some(_) => {
            return Err(SinkError::Usage(
                "--max-connections must be a whole number from 1 to 1024".into(),
            ))
        }
    };
    let tailnet = match flag(args, "--tailnet")? {
        Some(name) => Some(TailnetConfig {
            name: tailnet_name(name)?,
            // Beside the captures by default: the folder the sink already owns.
            devices: flag(args, "--devices")?
                .map_or_else(|| out.join("devices.json"), PathBuf::from),
            max_session: secs(
                "--tailnet-session-hours",
                DEFAULT_TAILNET_SESSION_HOURS,
                168,
            )? * 3600,
        }),
        None if flag(args, "--devices")?.is_some()
            || flag(args, "--tailnet-session-hours")?.is_some() =>
        {
            return Err(SinkError::Usage(
                "--devices and --tailnet-session-hours need --tailnet".into(),
            ))
        }
        None => None,
    };
    let token = serve_token(args, stdin)?;
    Ok(ServeConfig {
        token,
        port,
        out,
        origin,
        per_session,
        total_max,
        limits: Limits {
            hello,
            idle,
            max_connections,
        },
        tailnet,
    })
}

pub(crate) fn run_serve(args: &[String]) -> Result<(), SinkError> {
    // stdin is read here (`--token-stdin`) and never again.
    let ServeConfig {
        token,
        port,
        out,
        origin,
        per_session,
        total_max,
        limits,
        tailnet,
    } = serve_config(args, io::stdin().lock())?;
    let budget = Arc::new(Budget {
        per_session,
        total_max,
        total_used: AtomicU64::new(0),
    });
    let refusals = Arc::new(Refusals::default());
    fs::create_dir_all(&out).map_err(|e| SinkError::io("cannot create", Some(out.clone()), e))?;
    // Before the port is bound: a list of devices that cannot be read stops the start.
    let tailnet = tailnet.as_ref().map(Tailnet::open).transpose()?;

    // Loopback only. Binding anything else is not an option, not a flag.
    let listener = TcpListener::bind(("127.0.0.1", port))
        .map_err(|e| SinkError::io(format!("bind 127.0.0.1:{port} failed"), None, e))?;
    // The port actually bound: `--port 0` lets the system pick one, and this
    // line is the start signal a test (or a parent process) waits for.
    let port = listener
        .local_addr()
        .map_err(|e| SinkError::io("cannot read the bound address", None, e))?
        .port();
    println!(
        "streamtap-sink listening on ws://127.0.0.1:{port}  out={}  origin={origin}",
        out.display()
    );
    if let Some(t) = &tailnet {
        println!(
            "tailnet: https://{} for {} paired device(s); forward it with `tailscale serve --bg {port}` (never funnel)",
            t.name,
            t.paired()
        );
    }

    let active = Arc::new(AtomicUsize::new(0));
    let waiting = Arc::new(Waiting::default());
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                if active.fetch_add(1, Ordering::SeqCst) >= limits.max_connections {
                    active.fetch_sub(1, Ordering::SeqCst);
                    eprintln!(
                        "connection refused: {} already open (--max-connections)",
                        limits.max_connections
                    );
                    continue;
                }
                let slot = Slot(Arc::clone(&active));
                let entry = waiting.add();
                let token = token.clone();
                let out = out.clone();
                let origin = origin.clone();
                let budget = Arc::clone(&budget);
                let refusals = Arc::clone(&refusals);
                let tailnet = tailnet.clone();
                let spawned = std::thread::Builder::new().spawn(move || {
                    let _slot = slot;
                    if let Err(e) = handle_connection(
                        s,
                        &token,
                        &origin,
                        &out,
                        &budget,
                        &refusals,
                        limits,
                        entry,
                        tailnet.as_ref(),
                    ) {
                        eprintln!("connection ended with error: {e}");
                    }
                });
                if let Err(e) = spawned {
                    eprintln!("cannot start a connection thread: {e}");
                }
            }
            Err(e) => eprintln!("accept failed: {e}"),
        }
    }
    Ok(())
}

/// Handshake guard: the `Origin` header must equal `origin` exactly; anything
/// else (missing, a web page, another extension) gets 403 before any file is
/// touched.
// The error type is fixed by tungstenite's handshake `Callback` trait; it
// cannot be boxed.
#[allow(clippy::result_large_err)]
pub(crate) fn check_origin(
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
pub(crate) fn next_binary(
    ws: &mut tungstenite::WebSocket<Timed>,
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
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                let t = ws.get_ref();
                let what = if t.deadline.is_some() {
                    "no hello in time"
                } else if t.end_at.is_some_and(|end| end <= Instant::now()) {
                    "the connection reached its time limit (--tailnet-session-hours)"
                } else {
                    "idle too long"
                };
                eprintln!("{peer}: {what}, closing");
                return None;
            }
            Err(e) => {
                eprintln!("{peer}: connection error: {e}");
                return None;
            }
        }
    }
}

/// The WebSocket handshake (loopback origin check, frame caps) on a fresh
/// connection.
// The handshake closure returns tungstenite's fixed (large) error type.
#[allow(clippy::result_large_err)]
pub(crate) fn accept_ws(
    stream: TcpStream,
    origin: &str,
    limits: Limits,
    pushed_out: Arc<AtomicBool>,
    tailnet: Option<&Tailnet>,
) -> Result<(tungstenite::WebSocket<Timed>, std::net::SocketAddr, Class), SinkError> {
    let io_err = |what: &'static str| move |e: io::Error| SinkError::io(what, None, e);
    let peer = stream.peer_addr().map_err(io_err("peer address"))?;
    stream
        .set_write_timeout(Some(WRITE_TIMEOUT))
        .map_err(io_err("set write timeout"))?;
    let stream = Timed::new(stream, limits, pushed_out);
    // tungstenite's default caps a frame at 16 MiB; segments may be up to
    // MAX_MESSAGE, so the frame and message caps follow it (plus header).
    let config = tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE + MAX_HEADER + 4))
        .max_frame_size(Some(MAX_MESSAGE + MAX_HEADER + 4));
    let mut class = Class::Local;
    let mut ws = tungstenite::accept_hdr_with_config(
        stream,
        |req: &_, resp| match classify(tailnet, req) {
            Ok(Class::Local) => check_origin(origin, req, resp),
            Ok(Class::Remote) => {
                class = Class::Remote;
                check_remote_origin(req, resp)
            }
            Err(why) => {
                eprintln!("{peer}: refused: {why}");
                Err(forbidden(why))
            }
        },
        Some(config),
    )
    .map_err(|e| SinkError::Protocol(format!("{peer}: {e}")))?;
    // Until a device is logged in, its messages are a few hundred bytes.
    if class == Class::Remote {
        ws.set_config(|c| {
            c.max_message_size = Some(MAX_GATE_MESSAGE);
            c.max_frame_size = Some(MAX_GATE_MESSAGE);
        });
    }
    Ok((ws, peer, class))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_connection(
    stream: TcpStream,
    token: &Secret,
    origin: &str,
    out: &Path,
    budget: &Arc<Budget>,
    refusals: &Arc<Refusals>,
    limits: Limits,
    waiting: WaitingEntry,
    tailnet: Option<&Arc<Tailnet>>,
) -> Result<(), SinkError> {
    let mut stream = stream;
    // The phone's browser opening the pairing link: a page, no WebSocket.
    if let Some(t) = tailnet {
        if answer_plain_get(&mut stream, t, limits.hello, &waiting.pushed_out) != Plain::Handshake {
            return Ok(());
        }
    }
    let (mut ws, peer, class) = accept_ws(
        stream,
        origin,
        limits,
        Arc::clone(&waiting.pushed_out),
        tailnet.map(Arc::as_ref),
    )?;
    // A connection through the tailnet is a logged-in device from here on, or it is over.
    let _device = match (class, tailnet) {
        (Class::Local, _) => None,
        (Class::Remote, Some(t)) => {
            let Some(live) = t.gate(&mut ws, peer) else {
                return Ok(());
            };
            println!("{peer}: device {} logged in", live.device);
            ws.set_config(|c| {
                c.max_message_size = Some(MAX_MESSAGE + MAX_HEADER + 4);
                c.max_frame_size = Some(MAX_MESSAGE + MAX_HEADER + 4);
            });
            ws.get_mut().end_at = Some(Instant::now() + t.max_session);
            Some(live)
        }
        (Class::Remote, None) => return Ok(()),
    };
    let local = class == Class::Local;
    let door = Door {
        token,
        local,
        tailnet,
    };
    run_session(ws, peer, &door, out, budget, refusals, waiting)
}

/// Who may say what on a connection that passed the handshake.
pub(crate) struct Door<'a> {
    pub(crate) token: &'a Secret,
    /// From this machine: `hello` needs the token, and the command line may ask. Otherwise a logged-in device.
    pub(crate) local: bool,
    pub(crate) tailnet: Option<&'a Arc<Tailnet>>,
}

/// A first message that is not `hello`. The sink's own command line (`devices`) is answered: from this machine
/// only, with the token. Everything else ends the connection (a device pairs and logs in before the read loop).
fn before_hello(
    ws: &mut tungstenite::WebSocket<Timed>,
    peer: std::net::SocketAddr,
    door: &Door<'_>,
    header: Header,
) {
    match header {
        Header::Control { token, op, device } if door.local => {
            if door.token.matches(token.as_bytes()) {
                tell(ws, &control(door.tailnet, &op, &device));
            } else {
                eprintln!("{peer}: wrong token, closing");
            }
        }
        Header::Segment(_) | Header::Event { .. } => {
            eprintln!("{peer}: message before hello, closing");
        }
        _ => eprintln!("{peer}: a device message out of place, closing"),
    }
}

/// The read loop of a connection: `hello`, then segments and events until it ends.
pub(crate) fn run_session(
    mut ws: tungstenite::WebSocket<Timed>,
    peer: std::net::SocketAddr,
    door: &Door<'_>,
    out: &Path,
    budget: &Arc<Budget>,
    refusals: &Arc<Refusals>,
    waiting: WaitingEntry,
) -> Result<(), SinkError> {
    let token = door.token;
    let mut session: Option<Session> = None;
    let mut waiting = Some(waiting);
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
                // A device proved itself with its key; everything else needs the shared token.
                if door.local && !token.matches(t.as_bytes()) {
                    eprintln!("{peer}: wrong token, closing");
                    break;
                }
                // A spent process budget creates no further directories.
                let Some(s) = Session::open(out, &id, &url, &title, budget, refusals)
                    .map_err(|e| SinkError::io("open session", None, e))?
                else {
                    eprintln!("{peer}: --max-total-gib reached, closing");
                    break;
                };
                println!(
                    "{peer}: session {} ({})",
                    s.dir.display(),
                    printable(&url, 300)
                );
                session = Some(s);
                // In: the deadline is spent, silence is judged by the idle timeout now, and
                // no later connection can push this one out.
                ws.get_mut().worked();
                waiting.take();
            }
            (Header::Hello { .. }, Some(_)) => {
                eprintln!("{peer}: second hello, ignoring");
            }
            (h, None) => {
                before_hello(&mut ws, peer, door, h);
                break;
            }
            (Header::Segment(h), Some(s)) => {
                ws.get_mut().worked();
                if let Err(e) = s.write_segment(&h, payload) {
                    eprintln!("{}: write failed, closing: {e}", s.dir.display());
                    break;
                }
            }
            (Header::Event { name, sb }, Some(s)) => {
                ws.get_mut().worked();
                println!("{}", event_log_line(&s.dir, &name, sb));
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
            (_, Some(_)) => {
                eprintln!("{peer}: a device message out of place, closing");
                break;
            }
        }
    }

    if let Some(mut s) = session {
        s.finish();
        s.summary();
    }
    Ok(())
}

pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

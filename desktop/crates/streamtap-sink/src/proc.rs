//! Child processes (ffmpeg, ffprobe) under one frame (RS-04, RS-06,
//! ENG-01..03): no shell, an empty environment plus a fixed allow-list, the
//! program from a closed enum, stdin null, both pipes drained on their own
//! threads with byte limits (stdout capped, stderr keeps its tail), one deadline for pipes and exit, no console
//! window. Every path ends with the child killed or exited and reaped.

use std::ffi::OsString;
use std::fmt;
use std::io::{self, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use crate::error::SinkError;

/// Environment variables a child gets from the parent (ENG-01); everything
/// else is cleared. ffmpeg needs none of the parent's switches or secrets.
pub const ENV_ALLOW: &[&str] = &[
    "PATH",
    "PATHEXT",
    "SystemRoot",
    "windir",
    "TEMP",
    "TMP",
    "TMPDIR",
    "HOME",
    "USERPROFILE",
];

/// Bytes kept per pipe (ENG-03): more on stdout kills the child; stderr is
/// drained to its end and only the last `PIPE_LIMIT` bytes are kept, so a
/// long mux that logs many errors is not killed for it.
pub const PIPE_LIMIT: usize = 64 * 1024;
/// Bytes of stderr quoted in an error message (the tail).
pub const STDERR_QUOTE: usize = 4 * 1024;
/// How long reader threads may take after the child is gone (a grandchild
/// can hold a pipe open).
const READER_GRACE: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(20);

/// The programs the sink starts (ENG-02: the verb is never a string from
/// outside).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Ffmpeg,
    Ffprobe,
}

impl Tool {
    pub fn name(self) -> &'static str {
        match self {
            Tool::Ffmpeg => "ffmpeg",
            Tool::Ffprobe => "ffprobe",
        }
    }
}

impl fmt::Display for Tool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Which pipe hit its limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pipe {
    Stdout,
    Stderr,
}

impl Pipe {
    fn code(self) -> u8 {
        match self {
            Pipe::Stdout => 1,
            Pipe::Stderr => 2,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Pipe::Stdout),
            2 => Some(Pipe::Stderr),
            _ => None,
        }
    }
}

impl fmt::Display for Pipe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Pipe::Stdout => "stdout",
            Pipe::Stderr => "stderr",
        })
    }
}

/// Where each tool is found: its name on `PATH` by default; tests point it
/// elsewhere.
#[derive(Debug, Clone)]
pub struct Tools {
    pub ffmpeg: OsString,
    pub ffprobe: OsString,
}

impl Default for Tools {
    fn default() -> Self {
        Self {
            ffmpeg: Tool::Ffmpeg.name().into(),
            ffprobe: Tool::Ffprobe.name().into(),
        }
    }
}

impl Tools {
    pub fn program(&self, tool: Tool) -> &OsString {
        match tool {
            Tool::Ffmpeg => &self.ffmpeg,
            Tool::Ffprobe => &self.ffprobe,
        }
    }
}

/// One child to run.
pub struct Spec<'a> {
    pub tool: Tool,
    pub program: &'a OsString,
    pub args: Vec<OsString>,
    /// Variables beyond the allow-list (derived by the sink, not inherited).
    pub extra_env: Vec<(String, String)>,
    pub timeout: Duration,
}

/// A child that exited with success.
#[derive(Debug)]
pub struct Output {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Runs `spec` to its end. A non-zero exit is `SinkError::ToolFailed` with
/// the tail of stderr; a missing program `ToolMissing`.
pub fn run(spec: &Spec<'_>) -> Result<Output, SinkError> {
    let tool = spec.tool;
    let mut child = command(spec).spawn().map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            SinkError::ToolMissing { tool }
        } else {
            SinkError::ToolSpawn { tool, source: e }
        }
    })?;

    // 0: no pipe over its limit, else `Pipe::code` of the first one.
    let overflow = Arc::new(AtomicU8::new(0));
    let (tx, rx) = mpsc::channel::<(Pipe, Vec<u8>)>();
    for pipe in [Pipe::Stdout, Pipe::Stderr] {
        let reader: Box<dyn Read + Send> = match pipe {
            Pipe::Stdout => Box::new(child.stdout.take().expect("piped stdout")),
            Pipe::Stderr => Box::new(child.stderr.take().expect("piped stderr")),
        };
        let tx = tx.clone();
        let overflow = Arc::clone(&overflow);
        thread::spawn(move || {
            let buf = match pipe {
                // More stdout than the limit is a misbehaving tool: kill it.
                Pipe::Stdout => {
                    let (buf, over) = read_limited(reader, PIPE_LIMIT);
                    if over {
                        let _ = overflow.compare_exchange(
                            0,
                            pipe.code(),
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        );
                    }
                    buf
                }
                // A long run may log many errors: drain on, keep the tail.
                Pipe::Stderr => read_tail(reader, PIPE_LIMIT),
            };
            let _ = tx.send((pipe, buf));
        });
    }
    drop(tx);

    // One deadline for the exit and the pipes (ENG-03).
    let deadline = Instant::now() + spec.timeout;
    let status = wait_until(&mut child, deadline, &overflow);

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    // An exited child's pipes count against the same deadline (a grandchild
    // can hold them open); after a kill the readers get a short grace.
    let pipes_by = match status {
        Waited::Exited(_) => deadline.max(Instant::now() + POLL),
        _ => Instant::now() + READER_GRACE,
    };
    let mut drained = 0;
    while drained < 2 {
        let left = pipes_by.saturating_duration_since(Instant::now());
        let Ok((pipe, buf)) = rx.recv_timeout(left) else {
            break;
        };
        drained += 1;
        match pipe {
            Pipe::Stdout => stdout = buf,
            Pipe::Stderr => stderr = buf,
        }
    }

    // Also when the child exited right after a pipe overflowed.
    if let Some(pipe) = Pipe::from_code(overflow.load(Ordering::SeqCst)) {
        return Err(SinkError::ToolOutputLimit {
            tool,
            pipe,
            limit: PIPE_LIMIT,
        });
    }
    match status {
        Waited::Exited(_) if drained < 2 => Err(SinkError::ToolTimeout {
            tool,
            after: spec.timeout,
        }),
        Waited::Exited(st) if st.success() => Ok(Output { stdout, stderr }),
        Waited::Exited(st) => Err(SinkError::ToolFailed {
            tool,
            status: describe(st),
            stderr: quote_tail(&stderr, STDERR_QUOTE),
        }),
        Waited::Overflow(pipe) => Err(SinkError::ToolOutputLimit {
            tool,
            pipe,
            limit: PIPE_LIMIT,
        }),
        Waited::TimedOut => Err(SinkError::ToolTimeout {
            tool,
            after: spec.timeout,
        }),
        Waited::Error(e) => Err(SinkError::ToolSpawn { tool, source: e }),
    }
}

/// The child as `run` starts it: no shell, cleared environment plus the
/// allow-list, stdin null, both pipes captured, no console window.
fn command(spec: &Spec<'_>) -> Command {
    let mut cmd = Command::new(spec.program);
    cmd.args(&spec.args)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in ENV_ALLOW {
        if let Some(v) = std::env::var_os(name) {
            cmd.env(name, v);
        }
    }
    for (k, v) in &spec.extra_env {
        cmd.env(k, v);
    }
    no_window(&mut cmd);
    cmd
}

enum Waited {
    Exited(ExitStatus),
    TimedOut,
    /// This pipe went over its limit first.
    Overflow(Pipe),
    Error(io::Error),
}

/// Polls the child until it exits, the deadline passes or a pipe overflows;
/// in the latter cases kills and reaps it.
fn wait_until(child: &mut Child, deadline: Instant, overflow: &AtomicU8) -> Waited {
    loop {
        match child.try_wait() {
            Ok(Some(st)) => return Waited::Exited(st),
            Ok(None) => {}
            Err(e) => {
                kill(child);
                return Waited::Error(e);
            }
        }
        if let Some(pipe) = Pipe::from_code(overflow.load(Ordering::SeqCst)) {
            kill(child);
            return Waited::Overflow(pipe);
        }
        let now = Instant::now();
        if now >= deadline {
            kill(child);
            return Waited::TimedOut;
        }
        thread::sleep(POLL.min(deadline - now));
    }
}

fn kill(child: &mut Child) {
    // Kill fails only if it already exited; wait reaps it either way.
    let _ = child.kill();
    let _ = child.wait();
}

/// Reads up to `limit` bytes; `true` if there was more (then stops reading,
/// and the caller kills the child, which closes the pipe).
fn read_limited(mut r: impl Read, limit: usize) -> (Vec<u8>, bool) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk) {
            Ok(0) | Err(_) => return (buf, false),
            Ok(n) => {
                if buf.len() + n > limit {
                    let room = limit - buf.len();
                    buf.extend_from_slice(&chunk[..room]);
                    return (buf, true);
                }
                buf.extend_from_slice(&chunk[..n]);
            }
        }
    }
}

/// Reads to EOF and keeps the last `limit` bytes; the pipe stays drained, so
/// the child never blocks on it.
fn read_tail(mut r: impl Read, limit: usize) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                // Trimmed in batches: at most about 2 * limit held.
                if buf.len() > 2 * limit {
                    buf.drain(..buf.len() - limit);
                }
            }
        }
    }
    if buf.len() > limit {
        buf.drain(..buf.len() - limit);
    }
    buf
}

fn describe(st: ExitStatus) -> String {
    st.code()
        .map_or_else(|| st.to_string(), |c| format!("exit code {c}"))
}

/// The last `max` bytes of `bytes` as printable text (no terminal escapes),
/// marked when cut.
pub fn quote_tail(bytes: &[u8], max: usize) -> String {
    let start = bytes.len().saturating_sub(max);
    let text: String = String::from_utf8_lossy(&bytes[start..])
        .chars()
        .map(|c| {
            if c == '\n' || c == '\t' || (' '..='~').contains(&c) {
                c
            } else if c == '\r' {
                '\n'
            } else {
                '?'
            }
        })
        .collect();
    let text = text.trim();
    if start > 0 {
        format!("[... {start} bytes cut]\n{text}")
    } else {
        text.to_owned()
    }
}

#[cfg(windows)]
fn no_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn no_window(_cmd: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// The variable that turns `child_helper` from a no-op into a fake tool.
    const CHILD: &str = "STREAMTAP_TEST_CHILD";

    /// Not a test of its own: the other tests start this test binary with
    /// `--exact proc::tests::child_helper` and `STREAMTAP_TEST_CHILD` set,
    /// which makes it a child that sleeps, floods stderr, fails or prints
    /// its environment. Without the variable it does nothing.
    #[test]
    fn child_helper() {
        let Ok(mode) = std::env::var(CHILD) else {
            return;
        };
        match mode.as_str() {
            "sleep" => thread::sleep(Duration::from_secs(120)),
            "spam" => {
                let line = "x".repeat(1023);
                for _ in 0..1024 {
                    eprintln!("{line}");
                }
                eprintln!("spam: the last line");
            }
            "flood" => {
                use std::io::Write;
                let line = "z".repeat(1023);
                let mut out = io::stdout().lock();
                for _ in 0..1024 {
                    if writeln!(out, "{line}").is_err() {
                        break;
                    }
                }
                drop(out);
                // Stays alive: only the kill ends it within the test's bound.
                thread::sleep(Duration::from_secs(120));
            }
            "fail" => {
                eprintln!("{}", "y".repeat(10_000));
                eprintln!("boom: the last line");
                std::process::exit(3);
            }
            "env" => {
                for (k, _) in std::env::vars_os() {
                    println!("ENV:{}", k.to_string_lossy());
                }
            }
            other => panic!("unknown child mode {other}"),
        }
    }

    fn with_helper<T>(mode: &str, timeout: Duration, f: impl FnOnce(&Spec<'_>) -> T) -> T {
        let exe: OsString = std::env::current_exe().expect("test exe").into();
        f(&Spec {
            tool: Tool::Ffmpeg,
            program: &exe,
            args: ["--exact", "proc::tests::child_helper", "--nocapture"]
                .iter()
                .map(OsString::from)
                .collect(),
            extra_env: vec![(CHILD.to_owned(), mode.to_owned())],
            timeout,
        })
    }

    fn helper(mode: &str, timeout: Duration) -> Result<Output, SinkError> {
        with_helper(mode, timeout, run)
    }

    fn sleeping_child() -> Child {
        with_helper("sleep", Duration::ZERO, |s| command(s).spawn()).expect("spawn")
    }

    /// `wait_until` itself must kill and reap: with a no-op kill `run` would
    /// still report the error after the reader grace and orphan the child.
    fn assert_killed(child: &mut Child, started: Instant) {
        assert!(
            matches!(child.try_wait(), Ok(Some(_))),
            "the child is dead and reaped"
        );
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "killed, not waited for"
        );
    }

    #[test]
    fn wait_until_kills_the_child_at_the_deadline() {
        let t = Instant::now();
        let mut child = sleeping_child();
        let w = wait_until(
            &mut child,
            Instant::now() + Duration::from_secs(1),
            &AtomicU8::new(0),
        );
        assert!(matches!(w, Waited::TimedOut));
        assert_killed(&mut child, t);
    }

    #[test]
    fn wait_until_kills_the_child_on_overflow() {
        let t = Instant::now();
        let mut child = sleeping_child();
        let w = wait_until(
            &mut child,
            Instant::now() + Duration::from_secs(60),
            &AtomicU8::new(Pipe::Stdout.code()),
        );
        assert!(matches!(w, Waited::Overflow(Pipe::Stdout)));
        assert_killed(&mut child, t);
    }

    #[test]
    fn a_hanging_child_is_killed_at_the_deadline() {
        let t = Instant::now();
        let err = helper("sleep", Duration::from_secs(2)).unwrap_err();
        assert!(
            matches!(
                err,
                SinkError::ToolTimeout {
                    tool: Tool::Ffmpeg,
                    ..
                }
            ),
            "{err}"
        );
        assert!(
            t.elapsed() < Duration::from_secs(60),
            "killed, not waited for"
        );
    }

    #[test]
    fn a_flooding_child_is_killed_at_the_stdout_limit() {
        let t = Instant::now();
        let err = helper("flood", Duration::from_secs(60)).unwrap_err();
        assert!(
            matches!(
                err,
                SinkError::ToolOutputLimit {
                    pipe: Pipe::Stdout,
                    ..
                }
            ),
            "{err}"
        );
        assert!(
            t.elapsed() < Duration::from_secs(60),
            "killed, not waited for"
        );
    }

    /// A long mux that logs many errors is not killed for it: stderr is
    /// drained to its end and only the tail is kept.
    #[test]
    fn a_chatty_stderr_keeps_its_tail_and_does_not_kill() {
        let out = helper("spam", Duration::from_secs(60)).expect("spam child succeeds");
        assert!(out.stderr.len() <= PIPE_LIMIT, "{}", out.stderr.len());
        let text = String::from_utf8_lossy(&out.stderr);
        assert!(
            text.trim_end().ends_with("spam: the last line"),
            "the tail is kept"
        );
    }

    #[test]
    fn read_tail_keeps_the_last_bytes() {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(read_tail(&data[..], 1000), &data[data.len() - 1000..]);
        assert_eq!(read_tail(&b"short"[..], 1000), b"short");
    }

    #[test]
    fn a_failing_child_reports_status_and_a_bounded_stderr_tail() {
        let err = helper("fail", Duration::from_secs(60)).unwrap_err();
        let SinkError::ToolFailed { status, stderr, .. } = &err else {
            panic!("expected ToolFailed, got {err}");
        };
        assert_eq!(status, "exit code 3");
        assert!(stderr.contains("boom: the last line"), "{stderr}");
        assert!(stderr.starts_with("[... "), "cut is marked: {stderr}");
        assert!(stderr.len() <= STDERR_QUOTE + 40, "{}", stderr.len());
        assert!(err.to_string().contains("boom: the last line"));
    }

    #[test]
    fn only_the_allow_list_reaches_the_child() {
        let out = helper("env", Duration::from_secs(60)).expect("env child");
        let text = String::from_utf8_lossy(&out.stdout);
        let names: Vec<&str> = text
            .lines()
            .filter_map(|l| l.strip_prefix("ENV:"))
            .collect();
        assert!(
            names.iter().any(|n| n.eq_ignore_ascii_case("PATH")),
            "{names:?}"
        );
        for n in &names {
            assert!(
                *n == CHILD || ENV_ALLOW.iter().any(|a| a.eq_ignore_ascii_case(n)),
                "{n} leaked into the child"
            );
        }
    }

    #[test]
    fn a_missing_program_is_named() {
        let program = OsString::from("streamtap-no-such-tool-4711");
        let err = run(&Spec {
            tool: Tool::Ffprobe,
            program: &program,
            args: Vec::new(),
            extra_env: Vec::new(),
            timeout: Duration::from_secs(10),
        })
        .unwrap_err();
        assert!(
            matches!(
                err,
                SinkError::ToolMissing {
                    tool: Tool::Ffprobe
                }
            ),
            "{err}"
        );
        assert!(err.to_string().contains("ffprobe"));
    }

    #[test]
    fn quote_tail_cuts_and_strips_escapes() {
        assert_eq!(quote_tail(b"ok\x1b[31m\r\n", 100), "ok?[31m");
        let q = quote_tail(&[b'a'; 10_000], 16);
        assert!(q.starts_with("[... 9984 bytes cut]"), "{q}");
        assert!(q.ends_with(&"a".repeat(16)));
    }
}

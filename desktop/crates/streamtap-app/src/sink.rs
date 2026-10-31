//! The sink as a child process, and `streamtap-sink mux` jobs. Nothing is
//! started through a shell, and on Windows no console window opens.

use std::collections::VecDeque;
use std::env;
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::Serialize;

use crate::jobs::KillOnClose;

/// Name of the sink executable next to this one.
#[cfg(windows)]
const SINK_EXE: &str = "streamtap-sink.exe";
#[cfg(not(windows))]
const SINK_EXE: &str = "streamtap-sink";
/// Name of the native messaging host executable next to this one.
#[cfg(windows)]
const HOST_EXE: &str = "streamtap-host.exe";
#[cfg(not(windows))]
const HOST_EXE: &str = "streamtap-host";
#[cfg(windows)]
const FFMPEG_EXE: &str = "ffmpeg.exe";
#[cfg(not(windows))]
const FFMPEG_EXE: &str = "ffmpeg";

/// Lines of sink output kept for the UI.
pub(crate) const LOG_LINES: usize = 200;
const LOG_LINE_CHARS: usize = 500;
/// Longest line read in one piece; the rest of a longer one is a new line.
const MAX_READ_LINE: u64 = 16 * 1024;
/// Mux stderr kept (the end of it, where the cause is).
pub(crate) const MUX_TAIL_BYTES: usize = 4096;

/// How often a running mux is checked for its end.
const WAIT_POLL: Duration = Duration::from_millis(50);

/// `CREATE_NO_WINDOW`: no console window for a child of a GUI process.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A `Command` that opens no console window on Windows.
pub(crate) fn command(program: impl AsRef<OsStr>) -> Command {
    #[allow(unused_mut, reason = "mutated on Windows only")]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Where the sink executable may be: next to the app exe (installed). Debug
/// builds also look in the `release` and `debug` directories of the cargo
/// target dir and in the source tree (development); a release build never
/// runs a program from a sibling directory that is not its own.
fn sink_candidates(exe_dir: &Path, dev: bool) -> Vec<PathBuf> {
    candidates(SINK_EXE, exe_dir, dev)
}

fn candidates(name: &str, exe_dir: &Path, dev: bool) -> Vec<PathBuf> {
    let mut candidates = vec![exe_dir.join(name)];
    if dev {
        if let Some(target) = exe_dir.parent() {
            candidates.push(target.join("release").join(name));
            candidates.push(target.join("debug").join(name));
        }
        candidates.push(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/release")
                .join(name),
        );
    }
    candidates
}

/// Where the sink executable is (see `sink_candidates`).
pub(crate) fn find_sink() -> Option<PathBuf> {
    let exe = env::current_exe().ok()?;
    sink_candidates(exe.parent()?, cfg!(debug_assertions))
        .into_iter()
        .find(|p| p.is_file())
}

/// Where the native messaging host executable is (same places as the sink).
pub(crate) fn find_host() -> Option<PathBuf> {
    let exe = env::current_exe().ok()?;
    candidates(HOST_EXE, exe.parent()?, cfg!(debug_assertions))
        .into_iter()
        .find(|p| p.is_file())
}

/// `ffmpeg` on `PATH` (the sink looks there too).
pub(crate) fn find_ffmpeg() -> Option<PathBuf> {
    find_in_path(env::var_os("PATH")?.as_os_str(), FFMPEG_EXE)
}

fn find_in_path(path: &OsStr, file: &str) -> Option<PathBuf> {
    env::split_paths(path)
        .map(|d| d.join(file))
        .find(|p| p.is_file())
}

/// Arguments of `streamtap-sink serve`. The token is on the command line
/// because the sink has no other input yet (documented limit of the sink).
pub(crate) fn serve_args(port: u16, token: &str, out: &Path) -> Vec<std::ffi::OsString> {
    vec![
        "serve".into(),
        "--token".into(),
        token.into(),
        "--port".into(),
        port.to_string().into(),
        "--out".into(),
        out.as_os_str().to_owned(),
    ]
}

/// Reads `r` line by line (lossy UTF-8, long lines cut) until it ends, so a
/// child never blocks on a full pipe.
pub(crate) fn for_each_line(r: impl Read, mut f: impl FnMut(&str)) {
    let mut r = BufReader::new(r);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match r.by_ref().take(MAX_READ_LINE).read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let text = String::from_utf8_lossy(&buf);
                f(text.trim_end_matches(['\r', '\n']));
            }
        }
    }
}

/// A log line made safe to keep and show: the token never appears in it,
/// control characters are dropped and the length is bounded.
pub(crate) fn clean_line(line: &str, token: &str) -> String {
    let line = if token.is_empty() {
        line.to_owned()
    } else {
        line.replace(token, "<token>")
    };
    line.chars()
        .filter(|c| !c.is_control())
        .take(LOG_LINE_CHARS)
        .collect()
}

pub(crate) fn push_log(log: &mut VecDeque<String>, line: String) {
    if log.len() >= LOG_LINES {
        log.pop_front();
    }
    log.push_back(line);
}

/// The capture directory name from a sink line that reports a DRM refusal:
/// `<dir>: sbN carries encrypted media (DRM); ...` or
/// `<dir>: sbN was refused as DRM before this reconnect; ...`.
///
/// `<dir>` must be exactly `<root>/<capture id>`. Other sink lines carry page
/// text (the session line prints the page URL), so a page could otherwise
/// forge a refusal for any capture by putting `/<id>: sb1 carries ...` into
/// its URL; such a line has a prefix that is no path under `root`.
pub(crate) fn drm_capture_of(line: &str, root: &Path) -> Option<String> {
    let (dir, rest) = line.split_once(": sb")?;
    let reported =
        rest.contains("carries encrypted media (DRM)") || rest.contains("refused as DRM");
    if !reported {
        return None;
    }
    let dir = Path::new(dir);
    let name = crate::paths::validate_id(dir.file_name()?.to_str()?).ok()?;
    (dir.parent()? == root).then(|| name.to_owned())
}

/// What a `streamtap-sink mux` exit code means (README, "Exit codes").
pub(crate) fn mux_exit_meaning(code: Option<i32>) -> &'static str {
    match code {
        Some(0) => "ok",
        Some(1) => "a file or socket operation failed",
        Some(2) => "usage error (the app passed bad arguments)",
        Some(3) => "the capture holds nothing to mux",
        Some(4) => "ffmpeg is missing or cannot be started; install ffmpeg and put it on PATH",
        Some(5) => "a tool timed out or wrote too much output",
        Some(6) => "ffmpeg failed",
        Some(_) => "unexpected exit code",
        None => "the sink was stopped by a signal",
    }
}

/// A stable key for [`mux_exit_meaning`], which the UI translates (the
/// English `meaning` is its fallback).
pub(crate) fn mux_exit_reason(code: Option<i32>) -> &'static str {
    match code {
        Some(0) => "ok",
        Some(1) => "exit-1",
        Some(2) => "exit-2",
        Some(3) => "exit-3",
        Some(4) => "exit-4",
        Some(5) => "exit-5",
        Some(6) => "exit-6",
        Some(_) => "exit-unexpected",
        None => "signal",
    }
}

/// The last `max` bytes of `text`, cut at a character boundary.
pub(crate) fn tail(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub(crate) enum MuxState {
    Running,
    Ok,
    Failed {
        code: Option<i32>,
        /// Stable key of `meaning` for the UI's translations.
        reason: &'static str,
        meaning: String,
        stderr_tail: String,
    },
}

/// A running mux the app can abort: the child, the job that ties it (and the
/// ffmpeg it starts) to the app, and the half-written file it leaves behind
/// when it is killed.
#[derive(Debug)]
pub(crate) struct MuxHandle {
    child: Mutex<Child>,
    job: Mutex<Option<KillOnClose>>,
    part_file: Option<PathBuf>,
    /// `abort` ran: the exit status is then no outcome of the mux (killing a
    /// job ends its processes with exit code 0).
    aborted: AtomicBool,
    /// Why the child could not be tied to the app (it then outlives a hard
    /// kill of the app).
    job_error: Option<String>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl MuxHandle {
    /// Why the mux is not tied to the app, if it is not.
    pub(crate) fn job_error(&self) -> Option<&str> {
        self.job_error.as_deref()
    }

    /// Kills the mux and everything it started (ffmpeg), waits until the
    /// processes are gone and removes `out.part.mp4`, which the mux could not
    /// clean up itself because it was killed.
    pub(crate) fn abort(&self) {
        self.aborted.store(true, Ordering::SeqCst);
        // Closing the job ends the whole tree (the child alone on other platforms).
        lock(&self.job).take();
        let mut child = lock(&self.child);
        let _ = child.kill();
        let _ = child.wait();
        if let Some(part) = &self.part_file {
            let _ = std::fs::remove_file(part);
        }
    }
}

/// Runs `cmd` (a `streamtap-sink mux` command) to its end; stdout is
/// discarded, the end of stderr is kept. `register` gets the handle as soon
/// as the child runs. `tie` puts the child into a kill-on-close job
/// ([`KillOnClose::new`]); when it fails the handle says why. The caller runs
/// this on its own thread.
pub(crate) fn run_child(
    mut cmd: Command,
    part_file: Option<PathBuf>,
    tie: impl FnOnce(&Child) -> std::io::Result<KillOnClose>,
    register: impl FnOnce(&Arc<MuxHandle>),
) -> MuxState {
    let mut child = match cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return MuxState::Failed {
                code: None,
                reason: "spawn",
                meaning: "the sink could not be started".into(),
                stderr_tail: e.to_string(),
            }
        }
    };
    let (job, job_error) = match tie(&child) {
        Ok(job) => (Some(job), None),
        Err(e) => (None, Some(e.to_string())),
    };
    let stderr = child.stderr.take();
    let handle = Arc::new(MuxHandle {
        child: Mutex::new(child),
        job: Mutex::new(job),
        part_file,
        aborted: AtomicBool::new(false),
        job_error,
    });
    register(&handle);
    let mut stderr_text = String::new();
    if let Some(err) = stderr {
        for_each_line(err, |l| {
            stderr_text.push_str(&clean_line(l, ""));
            stderr_text.push('\n');
            // Bound the memory: keep about twice the tail.
            if stderr_text.len() > MUX_TAIL_BYTES * 2 {
                let keep = tail(&stderr_text, MUX_TAIL_BYTES).to_owned();
                stderr_text = keep;
            }
        });
    }
    // Polled, not waited for: `abort` needs the lock while the mux runs.
    let status = loop {
        match lock(&handle.child).try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => std::thread::sleep(WAIT_POLL),
            Err(e) => break Err(e),
        }
    };
    if handle.aborted.load(Ordering::SeqCst) {
        return MuxState::Failed {
            code: None,
            reason: "aborted",
            meaning: "the mux was aborted because the app exited".into(),
            stderr_tail: tail(&stderr_text, MUX_TAIL_BYTES).to_owned(),
        };
    }
    match status {
        Ok(status) if status.success() => MuxState::Ok,
        Ok(status) => MuxState::Failed {
            code: status.code(),
            reason: mux_exit_reason(status.code()),
            meaning: mux_exit_meaning(status.code()).into(),
            stderr_tail: tail(&stderr_text, MUX_TAIL_BYTES).to_owned(),
        },
        Err(e) => MuxState::Failed {
            code: None,
            reason: "wait",
            meaning: "waiting for the sink failed".into(),
            stderr_tail: e.to_string(),
        },
    }
}

/// Runs `sink mux <dir>`; see `run_child`.
pub(crate) fn run_mux(sink: &Path, dir: &Path, register: impl FnOnce(&Arc<MuxHandle>)) -> MuxState {
    let mut cmd = command(sink);
    cmd.arg("mux").arg(dir);
    // The sink writes the result to `out.part.mp4` first.
    run_child(
        cmd,
        Some(dir.join("out.part.mp4")),
        KillOnClose::new,
        register,
    )
}

/// Opens a file or folder with the system's default handler. The program is
/// started directly with the path as its only argument (no shell).
pub(crate) fn open_path(path: &Path) -> Result<(), String> {
    #[cfg(windows)]
    let program = "explorer.exe";
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(all(not(windows), not(target_os = "macos")))]
    let program = "xdg-open";
    command(program)
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("cannot open {}: {e}", path.display()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn drm_lines_name_their_capture() {
        let root = Path::new("captures-root");
        let line = |name: &str, what: &str| format!("{}: sb1 {what}", root.join(name).display());
        let a = line(
            "1730000000_abcd1234",
            "carries encrypted media (DRM); not capturing this track",
        );
        assert_eq!(
            drm_capture_of(&a, root).as_deref(),
            Some("1730000000_abcd1234")
        );
        let b = line(
            "1730000000_abcd1234-2",
            "was refused as DRM before this reconnect; not capturing it",
        );
        assert_eq!(
            drm_capture_of(&b, root).as_deref(),
            Some("1730000000_abcd1234-2")
        );
        assert_eq!(drm_capture_of(&line("1_x", "init not written"), root), None);
        assert_eq!(drm_capture_of("listening on ws://127.0.0.1:1", root), None);
        // Another folder, a non-capture name, a path that only ends like one.
        let other = format!(
            "{}: sb1 carries encrypted media (DRM)",
            Path::new("elsewhere").join("1_x").display()
        );
        assert_eq!(drm_capture_of(&other, root), None);
        assert_eq!(
            drm_capture_of(&line("src", "carries encrypted media (DRM)"), root),
            None
        );
        assert_eq!(
            drm_capture_of("C:: sb1 carries encrypted media (DRM)", root),
            None
        );
    }

    #[test]
    fn a_page_cannot_forge_a_drm_refusal_through_its_url() {
        // The sink prints `<peer>: session <dir> (<page url>)`; the URL is
        // page text and may contain what a DRM line looks like.
        let root = Path::new("captures-root");
        let session = format!(
            "127.0.0.1:5555: session {} (https://x/1730000000_victim: sb1 carries encrypted media (DRM))",
            root.join("1730000001_attack").display()
        );
        assert_eq!(drm_capture_of(&session, root), None);
        let slashed = format!(
            "127.0.0.1:5555: session {} (/1730000000_victim: sb1 carries encrypted media (DRM))",
            root.join("1730000001_attack").display()
        );
        assert_eq!(drm_capture_of(&slashed, root), None);
    }

    #[test]
    fn a_release_build_only_trusts_the_directory_of_its_own_exe() {
        let dir = Path::new("some").join("install");
        assert_eq!(sink_candidates(&dir, false), vec![dir.join(SINK_EXE)]);
        let dev = sink_candidates(&dir, true);
        assert_eq!(dev[0], dir.join(SINK_EXE));
        assert!(dev.len() > 1, "development builds look further");
    }

    #[test]
    fn every_exit_code_has_its_own_reason_key() {
        let keys: Vec<_> = (0..=6).map(|c| mux_exit_reason(Some(c))).collect();
        let unique: std::collections::HashSet<_> = keys.iter().collect();
        assert_eq!(unique.len(), keys.len(), "{keys:?}");
        assert!(!keys.contains(&"exit-unexpected"));
        assert_eq!(mux_exit_reason(Some(7)), "exit-unexpected");
        assert_eq!(mux_exit_reason(Some(-1)), "exit-unexpected");
        assert_eq!(mux_exit_reason(None), "signal");
    }

    #[test]
    fn every_exit_code_has_a_meaning() {
        for c in 0..=6 {
            assert_ne!(mux_exit_meaning(Some(c)), "unexpected exit code", "{c}");
        }
        assert_eq!(mux_exit_meaning(Some(7)), "unexpected exit code");
        assert!(mux_exit_meaning(Some(4)).contains("ffmpeg"));
        assert!(mux_exit_meaning(None).contains("signal"));
    }

    #[test]
    fn the_token_never_reaches_the_log() {
        let line = "echo secret-token-123 and again secret-token-123\u{7}";
        let cleaned = clean_line(line, "secret-token-123");
        assert!(!cleaned.contains("secret-token-123"));
        assert!(!cleaned.contains('\u{7}'));
        assert_eq!(clean_line(&"x".repeat(2000), "t").len(), LOG_LINE_CHARS);
    }

    #[test]
    fn the_log_is_a_ring() {
        let mut log = VecDeque::new();
        for i in 0..LOG_LINES + 5 {
            push_log(&mut log, i.to_string());
        }
        assert_eq!(log.len(), LOG_LINES);
        assert_eq!(log.front().map(String::as_str), Some("5"));
    }

    #[test]
    fn lines_survive_invalid_utf8_and_long_input() {
        let mut got = Vec::new();
        let mut data = b"one\r\ntw\xffo\n".to_vec();
        data.extend(std::iter::repeat_n(
            b'a',
            usize::try_from(MAX_READ_LINE).unwrap() + 10,
        ));
        data.extend_from_slice(b"\nlast");
        for_each_line(&data[..], |l| got.push(l.to_owned()));
        assert_eq!(got[0], "one");
        assert!(got[1].starts_with("tw") && got[1].ends_with('o'));
        assert!(
            got.len() >= 4,
            "a long line is split, reading goes on: {}",
            got.len()
        );
        assert_eq!(got.last().map(String::as_str), Some("last"));
    }

    #[test]
    fn tail_cuts_on_a_char_boundary() {
        assert_eq!(tail("abcdef", 3), "def");
        assert_eq!(tail("abc", 10), "abc");
        let s = "a\u{e4}\u{e4}\u{e4}"; // 1 + 3*2 bytes
        assert_eq!(tail(s, 3), "\u{e4}");
    }

    #[test]
    fn serve_args_carry_token_port_and_dir() {
        let a = serve_args(17650, "tok", Path::new("/c"));
        let s: Vec<String> = a.iter().map(|x| x.to_string_lossy().into_owned()).collect();
        assert_eq!(
            s,
            ["serve", "--token", "tok", "--port", "17650", "--out", "/c"]
        );
    }

    #[test]
    fn mux_of_a_missing_sink_fails_cleanly() {
        let state = run_mux(
            Path::new("does-not-exist-streamtap-sink"),
            Path::new("x"),
            |_| {},
        );
        assert!(matches!(state, MuxState::Failed { code: None, .. }));
    }

    #[cfg(windows)]
    #[test]
    fn an_aborted_mux_ends_at_once_and_leaves_no_part_file() {
        use std::sync::mpsc;
        let dir = std::env::temp_dir().join(format!("streamtap-app-abort-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("out.part.mp4");
        std::fs::write(&part, b"half a file").unwrap();
        let mut cmd = command("ping");
        cmd.args(["-n", "120", "127.0.0.1"]);
        let (tx, rx) = mpsc::channel();
        let part2 = part.clone();
        let runner = std::thread::spawn(move || {
            run_child(cmd, Some(part2), KillOnClose::new, |h| {
                tx.send(Arc::clone(h)).unwrap();
            })
        });
        let handle = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("registered");
        let t0 = std::time::Instant::now();
        handle.abort();
        let state = runner.join().unwrap();
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "the mux was not aborted"
        );
        assert!(matches!(state, MuxState::Failed { .. }), "{state:?}");
        assert!(!part.exists(), "the half-written file is removed");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A command that ends at once with exit code 0 (the test exe listing
    /// no tests).
    pub(crate) fn quick_command() -> Command {
        let mut cmd = command(env::current_exe().unwrap());
        cmd.args(["--exact", "no-such-test-streamtap", "--test-threads=1"]);
        cmd
    }

    #[test]
    fn a_mux_that_cannot_be_tied_to_the_app_says_why() {
        let mut job_error = None;
        let state = run_child(
            quick_command(),
            None,
            |_| Err(std::io::Error::other("job denied")),
            |h| job_error = h.job_error().map(str::to_owned),
        );
        assert!(matches!(state, MuxState::Ok), "{state:?}");
        assert_eq!(job_error.as_deref(), Some("job denied"));
        let mut job_error = Some(String::new());
        run_child(quick_command(), None, KillOnClose::new, |h| {
            job_error = h.job_error().map(str::to_owned);
        });
        assert_eq!(job_error, None, "a tied mux has nothing to say");
    }

    #[test]
    fn path_search_finds_a_file_in_a_listed_directory() {
        let dir = std::env::temp_dir().join(format!("streamtap-app-path-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("tool.bin"), b"").unwrap();
        let path = env::join_paths([Path::new("/nonexistent-dir"), dir.as_path()]).unwrap();
        assert_eq!(find_in_path(&path, "tool.bin"), Some(dir.join("tool.bin")));
        assert_eq!(find_in_path(&path, "other.bin"), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

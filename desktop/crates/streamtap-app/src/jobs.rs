//! Ties a child process (and everything it starts) to the lifetime of its
//! owner. On Windows a child is put into a job object with the
//! kill-on-close limit: when the last handle of the job closes, which also
//! happens when the app is killed hard, Windows terminates every process in
//! it. The Win32 calls are made by the vetted `win32job` crate, so this crate
//! stays free of `unsafe`. Other platforms have no such guard here (the app
//! ships for Windows only).

use std::io;
use std::process::Child;

/// Holds a child (and its descendants) alive only as long as it exists:
/// dropping it terminates them on Windows.
#[derive(Debug)]
pub(crate) struct KillOnClose {
    #[cfg(windows)]
    _job: win32job::Job,
}

impl KillOnClose {
    /// Puts `child` into a new job that kills its processes on close.
    #[cfg(windows)]
    pub(crate) fn new(child: &Child) -> io::Result<Self> {
        use std::os::windows::io::AsRawHandle;
        let to_io = |e: win32job::JobError| io::Error::other(e.to_string());
        let mut info = win32job::ExtendedLimitInfo::new();
        info.limit_kill_on_job_close();
        let job = win32job::Job::create_with_limit_info(&info).map_err(to_io)?;
        job.assign_process(child.as_raw_handle() as isize)
            .map_err(to_io)?;
        Ok(Self { _job: job })
    }

    #[cfg(not(windows))]
    #[allow(clippy::unnecessary_wraps, reason = "same signature as on Windows")]
    pub(crate) fn new(_child: &Child) -> io::Result<Self> {
        Ok(Self {})
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    const HELPER_ENV: &str = "STREAMTAP_JOB_TEST_OWNER";

    fn long_sleeper() -> Child {
        Command::new("ping")
            .args(["-n", "120", "127.0.0.1"])
            .stdout(Stdio::null())
            .spawn()
            .expect("ping")
    }

    fn pid_alive(pid: u32) -> bool {
        let out = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
            .expect("tasklist");
        String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
    }

    fn wait_gone(pid: u32) -> bool {
        let end = Instant::now() + Duration::from_secs(10);
        while Instant::now() < end {
            if !pid_alive(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    /// Runs as a separate process (see the test below): plays the app, ties
    /// a child to itself, reports its pid and then waits to be killed. In a
    /// normal test run it does nothing.
    #[test]
    fn owner_process_for_the_hard_kill_test() {
        if std::env::var_os(HELPER_ENV).is_none() {
            return;
        }
        let mut child = long_sleeper();
        let _guard = KillOnClose::new(&child).expect("job");
        // libtest has just printed "test name ... " without a newline.
        println!(
            "
CHILD {}",
            child.id()
        );
        let _ = child.wait();
    }

    #[test]
    fn dropping_the_guard_kills_the_child() {
        let mut child = long_sleeper();
        let pid = child.id();
        let guard = KillOnClose::new(&child).expect("job");
        assert!(pid_alive(pid));
        drop(guard);
        assert!(wait_gone(pid), "the child outlived its guard");
        let _ = child.wait();
    }

    #[test]
    fn a_hard_kill_of_the_owner_kills_its_children() {
        let mut owner = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "jobs::tests::owner_process_for_the_hard_kill_test",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(HELPER_ENV, "1")
            .stdout(Stdio::piped())
            .spawn()
            .expect("owner");
        let out = owner.stdout.take().unwrap();
        let pid: u32 = BufReader::new(out)
            .lines()
            .map_while(Result::ok)
            .find_map(|l| l.strip_prefix("CHILD ").and_then(|p| p.parse().ok()))
            .expect("the owner reports its child");
        assert!(pid_alive(pid));
        // TerminateProcess: no destructor of the owner runs.
        owner.kill().expect("kill owner");
        let _ = owner.wait();
        let gone = wait_gone(pid);
        if !gone {
            let _ = Command::new("taskkill")
                .args(["/F", "/PID", &pid.to_string()])
                .output();
        }
        assert!(gone, "the child survived a hard kill of its owner");
    }
}

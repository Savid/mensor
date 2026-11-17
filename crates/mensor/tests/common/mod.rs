//! What the tests that start the binaries share.

use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// A process a test started, stopped and waited for when the test ends,
/// however it ends.
pub(crate) struct Running(pub(crate) Child);

impl Running {
    /// Asks the process to stop, as a person or a service manager does,
    /// and waits up to `within` for it to end.
    fn ask(&mut self, within: Duration) -> Option<ExitStatus> {
        if let Ok(Some(status)) = self.0.try_wait() {
            return Some(status);
        }
        // Not reaped, so its id is still its own.
        let pid = self.0.id().to_string();
        let _ = Command::new("kill").args(["-TERM", &pid]).status();
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.0.try_wait() {
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }

    /// Asks the process to stop, and waits for it to end.
    pub(crate) fn terminate(&mut self) -> ExitStatus {
        let ended = self.ask(Duration::from_secs(10));
        ended.expect("the process stops when it is asked to")
    }
}

impl Drop for Running {
    /// Asks first, so that a hand ends what its commands left running, then
    /// kills.
    fn drop(&mut self) {
        if self.ask(Duration::from_secs(2)).is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

/// The binary `bin` with no environment but `PATH` and `vars`, its output
/// thrown away.
pub(crate) fn spawn(bin: &str, args: &[&str], vars: &[(&str, &str)]) -> Running {
    let child = Command::new(bin)
        .args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .envs(vars.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    Running(child)
}

/// A command that asks of the process that ran it: its id, then its
/// environment.
#[cfg(target_os = "linux")]
pub(crate) const PROBE: &str = "echo parent=$PPID; cat /proc/$PPID/environ";

/// A command that leaves a process running, and says its id.
#[cfg(target_os = "linux")]
pub(crate) const LEAVES: &str = "sleep 30 >/dev/null 2>&1 & echo leftover=$!";

/// The id of the process a [`LEAVES`] command said it left, in `text`,
/// which may hold the command itself too.
#[cfg(target_os = "linux")]
pub(crate) fn leftover(text: &str) -> u32 {
    text.split("leftover=")
        .skip(1)
        .find_map(|after| {
            let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        })
        .expect("the command said what it left")
}

/// Whether the process `pid` has ended, reaped or not yet.
#[cfg(target_os = "linux")]
pub(crate) fn gone(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| Some(stat.rsplit_once(')')?.1.trim_start().starts_with('Z')))
        .unwrap_or(true)
}

/// Waits up to five seconds for the process `pid` to end.
#[cfg(target_os = "linux")]
pub(crate) fn ends(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !gone(pid) {
        assert!(Instant::now() < deadline, "{pid} lived on");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Whether `test` cannot show what it is for, because it runs as root, who
/// reads every process however it is set. It says so on standard error, so
/// that the pass it leaves is not taken for one that tested something.
#[cfg(target_os = "linux")]
pub(crate) fn skipped_as_root(test: &str) -> bool {
    use std::os::unix::fs::MetadataExt;
    let root = std::fs::metadata("/proc/self").is_ok_and(|me| me.uid() == 0);
    if root {
        eprintln!("SKIPPED {test}: as root, which reads every process however it is set");
    }
    root
}

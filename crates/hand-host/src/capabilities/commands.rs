use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use mensor_hand::{CallFuture, Capability};
use mensor_protocol::{CapabilityDecl, OperationDecl, Reply};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use super::capture::Capture;

/// How long output is read after a command exits. A process the command left
/// running may hold its pipes open for as long as it lives.
const DRAIN: Duration = Duration::from_millis(250);

/// The `commands` capability: runs shell commands on the hand's machine.
///
/// Its one operation, `bash`, runs a command with `bash -c` in the hand's
/// directory, in a process group of its own. A command that is cancelled or
/// runs out of time is killed with everything it started. What a finished
/// command leaves running lives on until the last copy of these commands is
/// dropped, which is when its hand ends: then every group they started is
/// killed.
#[derive(Clone, Debug)]
pub struct Commands {
    directory: PathBuf,
    hidden: Vec<String>,
    started: Arc<Started>,
    /// The most bytes kept of each of a command's output streams.
    pub max_output: usize,
    /// How long a command may run when its call does not say.
    pub timeout: Duration,
}

impl Commands {
    /// Commands that run in `directory`.
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            hidden: Vec::new(),
            started: Arc::default(),
            max_output: 64 * 1024,
            timeout: Duration::from_mins(10),
        }
    }

    /// Keeps these environment variables out of every command. A hand's own
    /// credentials belong here: a command the model writes inherits whatever
    /// the hand's process has. This removes them from a command's
    /// environment only. It is no sandbox, and a command may still be able
    /// to read them from the hand's process where the machine allows it.
    #[must_use]
    pub fn hide(mut self, names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.hidden.extend(names.into_iter().map(Into::into));
        self
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Bash {
    command: String,
    timeout_ms: Option<u64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Ran {
    /// The exit code, when the command exited.
    #[serde(skip_serializing_if = "Option::is_none")]
    exit: Option<i32>,
    /// Whether the command was killed for running out of time.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    timed_out: bool,
    stdout: String,
    stderr: String,
    /// Whether part of the output was left out.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    truncated: bool,
}

impl Capability for Commands {
    fn declare(&self) -> CapabilityDecl {
        CapabilityDecl {
            name: "commands".into(),
            operations: vec![OperationDecl {
                name: "bash".into(),
                description: "Run a shell command with bash in the working directory. Returns \
                              its exit code, stdout and stderr."
                    .into(),
                input: json!({
                    "type": "object",
                    "properties": {
                        "command": {"type": "string", "description": "The command to run."},
                        "timeoutMs": {
                            "type": "integer",
                            "description": "Kill the command after this many milliseconds.",
                        },
                    },
                    "required": ["command"],
                }),
            }],
        }
    }

    fn call(&self, operation: &str, input: Value) -> CallFuture<'_> {
        let operation = operation.to_owned();
        Box::pin(async move {
            if operation != "bash" {
                return Reply::Error(format!("commands has no operation named {operation}"));
            }
            match mensor_hand::input::<Bash>(&operation, input) {
                Ok(bash) => self.bash(bash).await,
                Err(reply) => reply,
            }
        })
    }
}

impl Commands {
    async fn bash(&self, bash: Bash) -> Reply {
        let mut command = Command::new("bash");
        command
            .arg("-c")
            .arg(&bash.command)
            .current_dir(&self.directory)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        for name in &self.hidden {
            command.env_remove(name);
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => return Reply::Error(format!("the command could not start: {error}")),
        };
        if let Some(leader) = child.id() {
            self.started.add(leader);
        }

        let stdout = Arc::new(Mutex::new(Capture::new(self.max_output)));
        let stderr = Arc::new(Mutex::new(Capture::new(self.max_output)));
        let mut readers = Vec::new();
        if let Some(pipe) = child.stdout.take() {
            readers.push(tokio::spawn(read(pipe, Arc::clone(&stdout))));
        }
        if let Some(pipe) = child.stderr.take() {
            readers.push(tokio::spawn(read(pipe, Arc::clone(&stderr))));
        }
        // If this future is dropped, or the command runs out of time, the
        // guard kills the command and everything it started.
        let mut guard = Group {
            leader: child.id(),
            readers,
            armed: true,
        };

        let limit = bash.timeout_ms.map_or(self.timeout, Duration::from_millis);
        let (exit, timed_out) = match timeout(limit, child.wait()).await {
            Ok(Ok(status)) => {
                guard.armed = false;
                (status.code(), false)
            }
            Ok(Err(error)) => {
                return Reply::Error(format!("the command could not be waited on: {error}"));
            }
            Err(_) => {
                guard.kill();
                let _ = child.wait().await;
                (None, true)
            }
        };
        guard.drain().await;

        let (stdout, cut_out) = lock(&stdout).text();
        let (stderr, cut_err) = lock(&stderr).text();
        let ran = Ran {
            exit,
            timed_out,
            stdout,
            stderr,
            truncated: cut_out || cut_err,
        };
        Reply::Output(serde_json::to_value(ran).expect("a command's result is JSON"))
    }
}

async fn read(mut pipe: impl AsyncRead + Unpin, into: Arc<Mutex<Capture>>) {
    let mut buffer = vec![0u8; 16 * 1024];
    while let Ok(read) = pipe.read(&mut buffer).await {
        if read == 0 {
            break;
        }
        lock(&into).push(&buffer[..read]);
    }
}

fn lock(capture: &Mutex<Capture>) -> std::sync::MutexGuard<'_, Capture> {
    capture.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The process groups of the commands run, which every copy of a
/// [`Commands`] shares, killed when the last copy is dropped.
#[derive(Debug, Default)]
struct Started(Mutex<Vec<Leader>>);

/// A command's first process, whose id is its group's, and when it started.
/// A group keeps its id from being taken while any process is in it, but a
/// group that has ended frees it for another, which the start tells apart.
#[derive(Debug)]
struct Leader {
    pid: Pid,
    since: Option<u64>,
}

impl Started {
    /// Records the group `leader` leads, and forgets every group that has
    /// no process left.
    fn add(&self, leader: u32) {
        let Ok(pid) = i32::try_from(leader) else {
            return;
        };
        let mut groups = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        groups.retain(|group| killpg(group.pid, None).is_ok());
        groups.push(Leader {
            pid: Pid::from_raw(pid),
            since: started(leader),
        });
    }
}

impl Drop for Started {
    fn drop(&mut self) {
        let groups = self.0.get_mut().unwrap_or_else(PoisonError::into_inner);
        for group in groups.iter() {
            let taken = u32::try_from(group.pid.as_raw())
                .ok()
                .and_then(started)
                .is_some_and(|since| Some(since) != group.since);
            if !taken {
                let _ = killpg(group.pid, Signal::SIGKILL);
            }
        }
    }
}

/// When the process `pid` started, in clock ticks since boot, if it is
/// there.
#[cfg(target_os = "linux")]
fn started(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, fields) = stat.rsplit_once(')')?;
    // After the name: the state, the field numbered 3, then on to the
    // start, numbered 22.
    fields.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(not(target_os = "linux"))]
fn started(_: u32) -> Option<u64> {
    None
}

/// A command's process group, and the tasks that read its output.
struct Group {
    leader: Option<u32>,
    readers: Vec<JoinHandle<()>>,
    armed: bool,
}

impl Group {
    fn kill(&self) {
        let leader = self.leader.and_then(|leader| i32::try_from(leader).ok());
        if let Some(leader) = leader {
            let _ = killpg(Pid::from_raw(leader), Signal::SIGKILL);
        }
    }

    /// Gives the readers a moment to finish, then stops them.
    async fn drain(&mut self) {
        let readers = std::mem::take(&mut self.readers);
        let aborts: Vec<_> = readers.iter().map(JoinHandle::abort_handle).collect();
        let all = async {
            for reader in readers {
                let _ = reader.await;
            }
        };
        if timeout(DRAIN, all).await.is_err() {
            for abort in aborts {
                abort.abort();
            }
        }
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        if self.armed {
            self.kill();
        }
        for reader in &self.readers {
            reader.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    async fn bash(commands: &Commands, input: Value) -> Value {
        match commands.call("bash", input).await {
            Reply::Output(output) => output,
            Reply::Error(error) => panic!("bash failed: {error}"),
        }
    }

    fn here() -> (tempfile::TempDir, Commands) {
        let dir = tempfile::tempdir().unwrap();
        let commands = Commands::new(dir.path());
        (dir, commands)
    }

    #[tokio::test]
    async fn a_command_returns_its_exit_code_and_both_streams() {
        let (_dir, commands) = here();
        let ran = bash(
            &commands,
            json!({"command": "echo out; echo err >&2; exit 3"}),
        )
        .await;
        assert_eq!(
            ran,
            json!({"exit": 3, "stdout": "out\n", "stderr": "err\n"})
        );
    }

    #[tokio::test]
    async fn a_command_runs_in_the_hands_directory() {
        let (dir, commands) = here();
        bash(&commands, json!({"command": "echo hi > note.txt"})).await;
        assert_eq!(
            std::fs::read_to_string(dir.path().join("note.txt")).unwrap(),
            "hi\n"
        );
    }

    #[tokio::test]
    async fn a_command_out_of_time_is_killed_with_what_it_started() {
        let (dir, commands) = here();
        let started = Instant::now();
        let ran = bash(
            &commands,
            json!({"command": "(sleep 30; touch late) & echo started; sleep 30", "timeoutMs": 200}),
        )
        .await;
        assert_eq!(ran["timedOut"], json!(true));
        assert_eq!(ran["stdout"], json!("started\n"));
        assert!(ran.get("exit").is_none());
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(!dir.path().join("late").exists());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_dropped_call_kills_its_command() {
        let (dir, commands) = here();
        // The shell may exec its last command: the child it starts in the
        // background shows the whole group is killed.
        let started = "sleep 30 & echo $! > pid; wait";
        let mut call = Box::pin(commands.call("bash", json!({"command": started})));
        // Run the call until its command has started, then drop it.
        let pid = loop {
            tokio::select! {
                _ = &mut call => panic!("the command ended"),
                () = tokio::time::sleep(Duration::from_millis(10)) => {
                    let pid = std::fs::read_to_string(dir.path().join("pid")).unwrap_or_default();
                    if pid.ends_with('\n') {
                        break pid.trim().to_owned();
                    }
                }
            }
        };
        drop(call);
        until_gone(&[pid]).await;
    }

    #[tokio::test]
    async fn what_a_finished_command_leaves_running_does_not_hold_its_result() {
        let (dir, commands) = here();
        // The leftover waits for `go`, which comes only once the result is
        // back: a result that waited on it would never come.
        let leftover = "(until [ -e go ]; do sleep 0.01; done; touch later) & echo started";
        let ran = timeout(
            Duration::from_secs(1),
            bash(&commands, json!({"command": leftover})),
        )
        .await
        .expect("the result waited on the leftover");
        assert_eq!(
            (ran["exit"].clone(), ran["stdout"].clone()),
            (json!(0), json!("started\n"))
        );
        std::fs::write(dir.path().join("go"), "").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !dir.path().join("later").exists() {
            assert!(Instant::now() < deadline, "the leftover was killed");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Whether the process `pid` is gone: ended, if not yet reaped.
    #[cfg(target_os = "linux")]
    fn gone(pid: &str) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| Some(stat.rsplit_once(')')?.1.trim_start().starts_with('Z')))
            .unwrap_or(true)
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn what_commands_leave_running_ends_when_their_hand_does() {
        let (_dir, commands) = here();
        let leftover = "sleep 30 >/dev/null 2>&1 & echo $!";
        let first = bash(&commands, json!({"command": leftover})).await;
        let second = bash(&commands.clone(), json!({"command": leftover})).await;
        let pids: Vec<String> = [first, second]
            .iter()
            .map(|ran| ran["stdout"].as_str().unwrap().trim().to_owned())
            .collect();
        assert!(pids.iter().all(|pid| !gone(pid)), "{pids:?}");

        drop(commands);
        until_gone(&pids).await;
    }

    /// Waits until every process in `pids` is gone, five seconds at most.
    #[cfg(target_os = "linux")]
    async fn until_gone(pids: &[String]) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !pids.iter().all(|pid| gone(pid)) {
            assert!(Instant::now() < deadline, "{pids:?} lived on");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn a_hidden_variable_is_not_in_a_commands_environment() {
        // HOME stands in for a credential: bash gives it no default.
        if std::env::var_os("HOME").is_none() {
            return;
        }
        let (_dir, commands) = here();
        let seen = bash(&commands, json!({"command": "echo ${HOME:+set}"})).await;
        assert_eq!(seen["stdout"], json!("set\n"));

        let commands = commands.hide(["HOME"]);
        let hidden = bash(&commands, json!({"command": "echo ${HOME:-unset}"})).await;
        assert_eq!(hidden["stdout"], json!("unset\n"));
    }

    #[tokio::test]
    async fn long_output_keeps_its_two_ends() {
        let (_dir, mut commands) = here();
        commands.max_output = 20;
        let ran = bash(
            &commands,
            json!({"command": "printf '%s' 0123456789abcdefghijKLMNOPQRST"}),
        )
        .await;
        assert_eq!(
            ran["stdout"],
            json!("0123456789\n... 10 bytes omitted ...\nKLMNOPQRST")
        );
        assert_eq!(ran["truncated"], json!(true));
    }

    #[tokio::test]
    async fn bad_input_and_unknown_operations_are_errors() {
        let (_dir, commands) = here();
        let bad = commands.call("bash", json!({"cmd": "ls"})).await;
        assert!(matches!(bad, Reply::Error(error) if error.contains("not valid")));
        let unknown = commands.call("zsh", json!({})).await;
        assert!(matches!(unknown, Reply::Error(error) if error.contains("zsh")));
    }
}

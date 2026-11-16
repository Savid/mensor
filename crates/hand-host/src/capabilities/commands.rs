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
/// directory. A command that is cancelled or runs out of time is killed with
/// everything it started. What a finished command leaves running lives on.
#[derive(Clone, Debug)]
pub struct Commands {
    directory: PathBuf,
    hidden: Vec<String>,
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
    async fn a_command_runs_in_the_bodys_directory() {
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

    #[tokio::test]
    async fn a_dropped_call_kills_its_command() {
        let (dir, commands) = here();
        let call = commands.call("bash", json!({"command": "sleep 1; touch survived"}));
        assert!(timeout(Duration::from_millis(100), call).await.is_err());
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(!dir.path().join("survived").exists());
    }

    #[tokio::test]
    async fn what_a_finished_command_leaves_running_does_not_hold_its_result() {
        let (dir, commands) = here();
        let started = Instant::now();
        let ran = bash(
            &commands,
            json!({"command": "(sleep 2; touch later) & echo started"}),
        )
        .await;
        assert_eq!(
            (ran["exit"].clone(), ran["stdout"].clone()),
            (json!(0), json!("started\n"))
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the result waited on the leftover"
        );
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(dir.path().join("later").exists(), "the leftover was killed");
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

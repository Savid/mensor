//! Running a hand: the flags, and the hand they describe.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Args;
use mensor_hand::{Capabilities, Core, Limits};
use mensor_hand_host::capabilities::{Cloning, Commands, Disk, Files, Spawn, Started};
use mensor_hand_host::{BoxFuture, Dial, WebSocket, boot};
use mensor_protocol::Name;

use crate::{CREDENTIALS, stopped, token};

/// A hand that dials a mind's host.
#[derive(Args)]
pub struct Hand {
    /// Where hands attach on the mind's host.
    #[arg(long, default_value = "ws://127.0.0.1:7878/v1/attach")]
    url: String,
    /// The mind this hand joins. A name its host does not know starts one.
    #[arg(long)]
    mind: Name,
    /// This hand's name in its mind.
    #[arg(long = "hand")]
    name: Name,
    #[command(flatten)]
    does: Does,
}

/// What a hand can do, and where.
#[derive(Args, Clone)]
pub struct Does {
    /// The directory commands run in and relative paths start from.
    #[arg(long, default_value = ".")]
    dir: PathBuf,
    /// Where the hand's clones are made: a directory per mind, and in it a
    /// directory per hand. `<dir>.clones` unless set.
    #[arg(long)]
    clones: Option<PathBuf>,
    /// What the hand can do: `commands`, `files` and `clone`.
    #[arg(long, value_delimiter = ',', default_value = "commands,files,clone")]
    capabilities: Vec<String>,
}

impl Does {
    /// A hand that works in `dir`, can do `capabilities`, and makes its
    /// clones in `clones`, or beside `dir` when that is `None`.
    pub fn new(dir: impl Into<PathBuf>, clones: Option<PathBuf>, capabilities: &[&str]) -> Self {
        Self {
            dir: dir.into(),
            clones,
            capabilities: capabilities.iter().map(|&name| name.to_owned()).collect(),
        }
    }

    /// The directory the hand works in, made absolute.
    pub fn dir(&self) -> Result<PathBuf, String> {
        std::fs::canonicalize(&self.dir)
            .map_err(|error| format!("{} cannot be used: {error}", self.dir.display()))
    }

    /// Where the hand's clones are made.
    pub fn clones(&self) -> Result<PathBuf, String> {
        if let Some(clones) = &self.clones {
            return Ok(clones.clone());
        }
        let dir = self.dir()?;
        Ok(match dir.file_name() {
            Some(name) => {
                let mut name = name.to_os_string();
                name.push(".clones");
                dir.with_file_name(name)
            }
            None => dir.join(".clones"),
        })
    }

    /// The capabilities asked for. `clone` starts each clone through
    /// `spawn`.
    pub fn capabilities(&self, spawn: &Arc<dyn Spawn>) -> Result<Capabilities, String> {
        let dir = self.dir()?;
        let mut capabilities = Capabilities::new();
        for capability in &self.capabilities {
            capabilities = match capability.as_str() {
                "commands" => capabilities.with(Commands::new(&dir).hide(CREDENTIALS)),
                "files" => capabilities.with(Files::new(Disk::new(&dir))),
                "clone" => capabilities.with(Cloning::new(&dir, self.clones()?, Arc::clone(spawn))),
                other => return Err(format!("this hand has no capability named {other}")),
            };
        }
        Ok(capabilities)
    }

    /// The same, for a clone working in `dir`: its clones go where this
    /// hand's do.
    pub fn of_clone(&self, dir: PathBuf) -> Result<Self, String> {
        Ok(Self {
            dir,
            clones: Some(self.clones()?),
            capabilities: self.capabilities.clone(),
        })
    }
}

/// Runs a hand that dials a mind's host, until it ends or the process is
/// asked to stop.
pub async fn run(hand: Hand) -> Result<(), String> {
    let spawn: Arc<dyn Spawn> = Arc::new(Process {
        args: std::env::args().collect(),
        clones: hand.does.clones()?,
    });
    let capabilities = hand.does.capabilities(&spawn)?;
    let core = Core::new(hand.mind, hand.name, boot(), Limits::default());
    let dial = WebSocket::new(hand.url).token(token());
    alive(core, dial, capabilities).await
}

/// Runs a hand until it ends or the process is asked to stop.
pub async fn alive(
    core: Core,
    dial: impl Dial + 'static,
    capabilities: Capabilities,
) -> Result<(), String> {
    let config = mensor_hand_host::Config::new(dial, capabilities);
    let (handle, task) = mensor_hand_host::start(core, config);
    tokio::pin!(task);
    loop {
        tokio::select! {
            ended = &mut task => {
                return match ended {
                    Ok(ended) => ended.map_err(|error| error.to_string()),
                    Err(error) => Err(format!("the hand stopped: {error}")),
                };
            }
            () = stopped() => handle.stop(),
        }
    }
}

/// Starts each clone as a process of its own: this program again, with
/// this hand's own arguments but for its mind, its name and its
/// directory. The host's token reaches it in this process's environment,
/// which the commands a hand runs are not given.
struct Process {
    args: Vec<String>,
    clones: PathBuf,
}

impl Spawn for Process {
    fn spawn(
        &self,
        mind: Name,
        hand: Name,
        dir: PathBuf,
    ) -> BoxFuture<'_, Result<Box<dyn Started>, String>> {
        Box::pin(async move {
            let program = std::env::current_exe()
                .map_err(|error| format!("this program cannot be found: {error}"))?;
            let set = [
                ("--mind", mind.to_string()),
                ("--hand", hand.to_string()),
                ("--dir", dir.display().to_string()),
                ("--clones", self.clones.display().to_string()),
            ];
            let args = with_flags(self.args.iter().skip(1).cloned(), &set);
            let log = log_beside(&dir)?;
            let child = tokio::process::Command::new(program)
                .args(args)
                .stdin(std::process::Stdio::null())
                .stdout(log.try_clone().map_err(|error| error.to_string())?)
                .stderr(log)
                .process_group(0)
                .spawn()
                .map_err(|error| format!("the clone could not be started: {error}"))?;
            tracing::info!(%mind, %hand, dir = %dir.display(), "a clone was started");
            Ok(Box::new(Child(child)) as Box<dyn Started>)
        })
    }
}

struct Child(tokio::process::Child);

impl Started for Child {
    fn stop(mut self: Box<Self>) {
        let _ = self.0.start_kill();
    }
}

/// The log of a clone working in `dir`: `<dir>.log`, beside it.
fn log_beside(dir: &Path) -> Result<std::fs::File, String> {
    let mut path = dir.as_os_str().to_os_string();
    path.push(".log");
    std::fs::File::create(&path)
        .map_err(|error| format!("{} cannot be written: {error}", dir.display()))
}

/// `args` with each flag in `set` given its value instead, whether it was
/// written `--flag value` or `--flag=value`.
fn with_flags(args: impl IntoIterator<Item = String>, set: &[(&str, String)]) -> Vec<String> {
    let mut kept = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let named = set.iter().find(|(flag, _)| {
            arg == *flag
                || arg
                    .strip_prefix(flag)
                    .is_some_and(|rest| rest.starts_with('='))
        });
        match named {
            Some((flag, _)) if arg == *flag => {
                args.next();
            }
            Some(_) => {}
            None => kept.push(arg),
        }
    }
    for (flag, value) in set {
        kept.push((*flag).to_owned());
        kept.push(value.clone());
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clone_is_given_its_own_flags_and_keeps_the_rest() {
        let args = [
            "hand",
            "--url",
            "ws://h/v1/attach",
            "--mind",
            "m",
            "--hand=a1",
            "--dir",
            ".",
        ]
        .map(String::from);
        let set = [
            ("--mind", "m.k3".into()),
            ("--hand", "a1".into()),
            ("--dir", "/c/m.k3/a1".into()),
        ];
        assert_eq!(
            with_flags(args, &set),
            [
                "hand",
                "--url",
                "ws://h/v1/attach",
                "--mind",
                "m.k3",
                "--hand",
                "a1",
                "--dir",
                "/c/m.k3/a1"
            ]
        );
    }
}

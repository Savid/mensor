//! What the `mensor` binaries are built from, beyond the hosts themselves.
//!
//! `mensor` hosts minds, runs a hand, or does both in one process.
//! `mensor-mind` only hosts minds and `mensor-hand` only runs a hand: each
//! is built from its half of this crate, and holds nothing of the other.

pub mod hand;
pub mod host;
pub mod local;

use std::future::Future;
use std::process::ExitCode;

use tokio::signal::unix::{SignalKind, signal};

/// The environment variable that holds the host's bearer token. A token is
/// never taken from a flag, where every user of the machine could read it.
pub const TOKEN: &str = "MENSOR_TOKEN";

/// The environment variable that names a Postgres database to keep every
/// history in. Its URL holds the database's password, so it is never a flag.
pub const POSTGRES: &str = "MENSOR_POSTGRES";

/// The variables no command a hand runs is given: the host's token, and the
/// model's credentials and the store's URL when the hand shares a process
/// with its mind.
pub const CREDENTIALS: [&str; 5] = [
    TOKEN,
    POSTGRES,
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "OPENAI_API_KEY",
];

/// The host's token, if this process was given one.
pub fn token() -> Option<String> {
    std::env::var(TOKEN).ok().filter(|token| !token.is_empty())
}

/// Readies a binary, first thing in `main`, before its runtime starts a
/// thread or anything reads a credential: closes the process to its user's
/// other processes, then turns logs on, and warns if it could not close it.
pub fn start() {
    let closed = undumpable();
    log();
    if !closed {
        // Without the errno, which would cost the binary 4 KB to tell.
        tracing::warn!("this process stays readable by its user's other processes");
    }
}

/// Keeps this user's other processes, the commands a hand runs among them,
/// from reading this process's environment and memory, where the host's
/// token and the model's credentials are: whether it did, or had nothing to
/// do. A command runs a program of its own, which they can read again.
///
/// Only on Linux. Elsewhere, and on Linux for root with `CAP_SYS_PTRACE`,
/// the process stays readable. Until it is called, so is this one: a
/// process of the same user that watches for it to start can read it.
fn undumpable() -> bool {
    #[cfg(target_os = "linux")]
    return nix::sys::prctl::set_dumpable(false).is_ok();
    #[cfg(not(target_os = "linux"))]
    true
}

/// Runs `work` on a runtime `runtime` builds, and ends the runtime with
/// everything still on it once `work` is done.
pub fn block_on(
    mut runtime: tokio::runtime::Builder,
    work: impl Future<Output = Result<(), String>>,
) -> Result<(), String> {
    let runtime = runtime
        .enable_all()
        .build()
        .map_err(|error| format!("the runtime could not start: {error}"))?;
    runtime.block_on(work)
}

/// Sends logs to standard error, at `info` unless `MENSOR_LOG` names
/// another level: `trace`, `debug`, `warn`, `error` or `off`.
fn log() {
    let level = std::env::var("MENSOR_LOG").ok();
    let level = level.and_then(|level| level.parse().ok());
    tracing_subscriber::fmt()
        .with_max_level(level.unwrap_or(tracing_subscriber::filter::LevelFilter::INFO))
        .with_writer(std::io::stderr)
        .init();
}

/// How a binary exits once its command has ended. An error is told to the
/// person who ran it.
pub fn exit(ended: Result<(), String>) -> ExitCode {
    match ended {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("mensor: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Resolves when the process is asked to stop.
pub async fn stopped() {
    let mut terminate = signal(SignalKind::terminate()).ok();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        () = async {
            match terminate.as_mut() {
                Some(terminate) => { terminate.recv().await; }
                None => std::future::pending().await,
            }
        } => {}
    }
}

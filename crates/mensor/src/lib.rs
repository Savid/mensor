//! What the `mensor` binaries are built from, beyond the hosts themselves.
//!
//! `mensor` hosts minds, runs a body, or does both in one process.
//! `mensor-mind` only hosts minds and `mensor-body` only runs a body: each
//! is built from its half of this crate, and holds nothing of the other.

pub mod body;
pub mod host;
pub mod local;

use std::process::ExitCode;

use tokio::signal::unix::{SignalKind, signal};

/// The environment variable that holds the host's bearer token. A token is
/// never taken from a flag, where every user of the machine could read it.
pub const TOKEN: &str = "MENSOR_TOKEN";

/// The variables no command a body runs is given: the host's token, and the
/// model's credentials when the body shares a process with its mind.
pub const CREDENTIALS: [&str; 4] = [
    TOKEN,
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "OPENAI_API_KEY",
];

/// The host's token, if this process was given one.
pub fn token() -> Option<String> {
    std::env::var(TOKEN).ok().filter(|token| !token.is_empty())
}

/// Sends logs to standard error, at `info` unless `MENSOR_LOG` names
/// another level: `trace`, `debug`, `warn`, `error` or `off`.
pub fn log() {
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

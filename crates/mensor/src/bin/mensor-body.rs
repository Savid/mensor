//! Run a body on this machine, and dial a mind's host. This is
//! `mensor body` alone, with nothing of a mind in it: no model client, no
//! database and no server.

use std::process::ExitCode;

use clap::Parser;
use mensor::body::Body;

#[derive(Parser)]
#[command(
    name = "mensor-body",
    version,
    about = "Run a body on this machine, and dial a mind's host"
)]
struct Cli {
    #[command(flatten)]
    body: Body,
}

/// One thread: a body waits on its mind and on the commands it runs, and
/// those are processes of their own.
#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    mensor::log();
    mensor::exit(mensor::body::run(Cli::parse().body).await)
}

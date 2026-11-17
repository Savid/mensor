//! Run a hand on this machine, and dial a mind's host. This is
//! `mensor hand` alone, with nothing of a mind in it: no model client, no
//! database and no server.

use std::process::ExitCode;

use clap::Parser;
use mensor::hand::Hand;

#[derive(Parser)]
#[command(
    name = "mensor-hand",
    version,
    about = "Run a hand on this machine, and dial a mind's host"
)]
struct Cli {
    #[command(flatten)]
    hand: Hand,
}

/// One thread: a hand waits on its mind and on the commands it runs, and
/// those are processes of their own.
fn main() -> ExitCode {
    mensor::start();
    let hand = Cli::parse().hand;
    let runtime = tokio::runtime::Builder::new_current_thread();
    mensor::exit(mensor::block_on(runtime, mensor::hand::run(hand)))
}

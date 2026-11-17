//! Host minds: serve the endpoint hands attach to, and the API. This is
//! `mensor serve` alone, with nothing of a hand in it.

use std::process::ExitCode;

use clap::Parser;
use mensor::host::Hosting;

#[derive(Parser)]
#[command(
    name = "mensor-mind",
    version,
    about = "Host minds: serve the endpoint hands attach to, and the API"
)]
struct Cli {
    #[command(flatten)]
    hosting: Hosting,
}

fn main() -> ExitCode {
    mensor::start();
    let hosting = Cli::parse().hosting;
    let runtime = tokio::runtime::Builder::new_multi_thread();
    mensor::exit(mensor::block_on(runtime, mensor::host::serve(hosting)))
}

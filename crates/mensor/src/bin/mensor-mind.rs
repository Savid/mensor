//! Host minds: serve the endpoint bodies attach to, and the API. This is
//! `mensor serve` alone, with nothing of a body in it.

use std::process::ExitCode;

use clap::Parser;
use mensor::host::Hosting;

#[derive(Parser)]
#[command(
    name = "mensor-mind",
    version,
    about = "Host minds: serve the endpoint bodies attach to, and the API"
)]
struct Cli {
    #[command(flatten)]
    hosting: Hosting,
}

#[tokio::main]
async fn main() -> ExitCode {
    mensor::log();
    mensor::exit(mensor::host::serve(Cli::parse().hosting).await)
}

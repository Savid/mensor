//! Run a mind's host, a body that dials one, or both in one process.

use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use mensor::body::{Body, Does, alive};
use mensor::host::{Hosting, open};
use mensor::local::Local;
use mensor_body::{Core, Limits};
use mensor_body_host::boot;
use mensor_protocol::Name;

#[derive(Parser)]
#[command(
    name = "mensor",
    version,
    about = "Run a mind's host, a body that attaches to one, or both together"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Host minds: serve the endpoint bodies attach to, and the API.
    Serve(Hosting),
    /// Run a body on this machine, and dial a mind's host.
    Body(Body),
    /// Host minds and run one body, in this one process.
    ///
    /// The body's commands run on this machine as this user, beside a
    /// process that holds the model's credentials. Use it on a machine you
    /// trust the model with.
    Run(Run),
}

#[derive(Args)]
struct Run {
    #[command(flatten)]
    host: Hosting,
    /// The body's name, and so the pair's.
    #[arg(long, default_value = "local")]
    name: Name,
    #[command(flatten)]
    does: Does,
}

#[tokio::main]
async fn main() -> ExitCode {
    mensor::log();
    mensor::exit(match Cli::parse().command {
        Command::Serve(hosting) => mensor::host::serve(hosting).await,
        Command::Body(body) => mensor::body::run(body).await,
        Command::Run(run) => self::run(run).await,
    })
}

async fn run(run: Run) -> Result<(), String> {
    let capabilities = run.does.capabilities()?;
    let (host, listener) = open(run.host).await?;
    let core = Core::new(run.name, boot(), Limits::default());
    let dial = Local::new(host.clone());
    tokio::select! {
        served = host.serve(listener, std::future::pending()) => {
            served.map_err(|error| format!("the host stopped: {error}"))
        }
        ended = alive(core, dial, capabilities, None) => ended,
    }
}

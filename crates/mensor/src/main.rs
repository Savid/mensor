//! Run a mind's host, a hand that dials one, or both in one process.

use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use mensor::hand::{Does, Hand, alive};
use mensor::host::{Hosting, open};
use mensor::local::Local;
use mensor_hand::{Core, Limits};
use mensor_hand_host::boot;
use mensor_protocol::Name;

#[derive(Parser)]
#[command(
    name = "mensor",
    version,
    about = "Run a mind's host, a hand that attaches to one, or both together"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Host minds: serve the endpoint hands attach to, and the API.
    Serve(Hosting),
    /// Run a hand on this machine, and dial a mind's host.
    Hand(Hand),
    /// Host minds and run one hand, in this one process.
    ///
    /// The hand's commands run on this machine as this user, beside a
    /// process that holds the model's credentials. Use it on a machine you
    /// trust the model with.
    Run(Run),
}

#[derive(Args)]
struct Run {
    #[command(flatten)]
    host: Hosting,
    /// The hand's name, and so the pair's.
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
        Command::Hand(hand) => mensor::hand::run(hand).await,
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

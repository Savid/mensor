//! Run a mind's host, a hand that dials one, or both in one process.

use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use mensor::hand::{Does, Hand};
use mensor::host::{Hosting, open};
use mensor::local::InProcess;
use mensor::stopped;
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
    /// Host minds and run one hand, named `local`, in this one process.
    ///
    /// The hand's commands run on this machine as this user, beside a
    /// process that holds the model's credentials. Use it on a machine you
    /// trust the model with. Its clones run in this process too.
    Run(Run),
}

#[derive(Args)]
struct Run {
    #[command(flatten)]
    host: Hosting,
    /// The mind the hand joins.
    #[arg(long, default_value = "main")]
    mind: Name,
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

/// Serves until the process is asked to stop. The hand may end before
/// then, when its mind lets it go, and the host serves on.
async fn run(run: Run) -> Result<(), String> {
    let (host, listener) = open(run.host).await?;
    let hands = InProcess::new(host.clone(), run.does.clone());
    let local = Name::new("local").map_err(|error| error.to_string())?;
    let hand = hands.start(run.mind, local, &run.does)?;
    tokio::select! {
        served = host.serve(listener, std::future::pending()) => {
            served.map_err(|error| format!("the host stopped: {error}"))
        }
        () = stopped() => {
            hand.stop();
            Ok(())
        }
    }
}

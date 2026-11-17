//! Run a mind's host, a hand that dials one, or both in one process.

use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use mensor::hand::{Does, Hand, Hands};
use mensor::host::{Hosting, open};
use mensor::local::Local;
use mensor::stopped;
use mensor_mind_host::mind_token;
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
    /// trust the model with. Its clones run in this process too, and start
    /// again with it.
    Run(Run),
    /// Print a token for one mind, made from the host's token in
    /// `MENSOR_TOKEN`.
    ///
    /// It reaches that mind and its children, and no other: give it to
    /// their hands in place of the host's. A host with a new token refuses
    /// every token made from the old one.
    Token(Token),
}

#[derive(Args)]
struct Token {
    /// The mind the token is for, which has no '.'. Its children are
    /// reached too.
    #[arg(long)]
    mind: Name,
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

fn main() -> ExitCode {
    mensor::start();
    let command = Cli::parse().command;
    let runtime = tokio::runtime::Builder::new_multi_thread();
    mensor::exit(mensor::block_on(runtime, async move {
        match command {
            Command::Serve(hosting) => mensor::host::serve(hosting).await,
            Command::Hand(hand) => mensor::hand::run(hand).await,
            Command::Run(run) => self::run(run).await,
            Command::Token(token) => self::token(&token),
        }
    }))
}

/// Prints the token: that is what it is asked for.
fn token(token: &Token) -> Result<(), String> {
    let host = mensor::token()
        .and_then(mensor_mind_host::Token::new)
        .ok_or_else(|| format!("{} holds no host token to make one from", mensor::TOKEN))?;
    let made = mind_token(&host, &token.mind).map_err(|error| error.to_string())?;
    println!("{made}");
    Ok(())
}

/// Serves until the process is asked to stop. A hand may end before then,
/// when its mind lets it go, and the host serves on.
async fn run(run: Run) -> Result<(), String> {
    if !run.mind.is_root() {
        return Err(format!(
            "{} cannot start a mind: a mind's name has no '.' and is at most {} bytes",
            run.mind,
            Name::ROOT_MAX_LEN
        ));
    }
    let (host, listener) = open(run.host).await?;
    let hands = Hands::new(Local::new(host.clone()), run.does);
    let local = Name::new("local").map_err(|error| error.to_string())?;
    hands.start(run.mind, local)?;
    tokio::select! {
        served = host.serve(listener, std::future::pending()) => {
            served.map_err(|error| format!("the host stopped: {error}"))
        }
        () = stopped() => {
            hands.stop();
            hands.ended().await
        }
    }
}

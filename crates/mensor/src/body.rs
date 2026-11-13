//! Running a body: the flags, and the body they describe.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Args;
use mensor_body::{Capabilities, Core, Limits};
use mensor_body_host::capabilities::{Commands, Disk, Files};
use mensor_body_host::{Dial, NameSource, WebSocket, boot};
use mensor_protocol::Name;
use tokio::signal::unix::{SignalKind, signal};

use crate::{CREDENTIALS, stopped, token};

/// A body that dials a mind's host.
#[derive(Args)]
pub struct Body {
    /// Where bodies attach on the mind's host.
    #[arg(long, default_value = "ws://127.0.0.1:7878/v1/attach")]
    url: String,
    /// The body's name.
    #[arg(long, required_unless_present = "name_file")]
    name: Option<Name>,
    /// A file holding the body's name, read before every dial. Whoever
    /// clones this body writes the clone's new name there; SIGHUP makes the
    /// body read it at once.
    #[arg(long)]
    name_file: Option<PathBuf>,
    /// The body this one is a copy of, when it is a copy made without its
    /// memory: its files were copied and this is a new process.
    #[arg(long)]
    cloned_from: Option<Name>,
    #[command(flatten)]
    does: Does,
}

/// What a body can do, and where.
#[derive(Args)]
pub struct Does {
    /// The directory commands run in and relative paths start from.
    #[arg(long, default_value = ".")]
    dir: PathBuf,
    /// What the body can do.
    #[arg(long, value_delimiter = ',', default_value = "commands,files")]
    capabilities: Vec<String>,
}

impl Does {
    /// The capabilities asked for.
    pub fn capabilities(&self) -> Result<Capabilities, String> {
        let mut capabilities = Capabilities::new();
        for capability in &self.capabilities {
            capabilities = match capability.as_str() {
                "commands" => capabilities.with(Commands::new(&self.dir).hide(CREDENTIALS)),
                "files" => capabilities.with(Files::new(Disk::new(&self.dir))),
                other => return Err(format!("this body has no capability named {other}")),
            };
        }
        Ok(capabilities)
    }
}

/// Runs a body that dials a mind's host, until it ends or the process is
/// asked to stop.
pub async fn run(body: Body) -> Result<(), String> {
    let source = body.name_file.clone().map(name_source);
    let name = match (&body.name, &source) {
        (Some(name), _) => name.clone(),
        (None, Some(source)) => source().ok_or("the name file holds no usable name")?,
        (None, None) => return Err("give the body a --name or a --name-file".into()),
    };
    let core = match body.cloned_from {
        Some(original) => {
            let mut core = Core::new(original, boot(), Limits::default());
            core.rename(name);
            core
        }
        None => Core::new(name, boot(), Limits::default()),
    };
    let dial = WebSocket::new(body.url).token(token());
    alive(core, dial, body.does.capabilities()?, source).await
}

/// Runs a body until it ends or the process is asked to stop. SIGHUP makes
/// it dial again, reading its name first.
pub async fn alive(
    core: Core,
    dial: impl Dial + 'static,
    capabilities: Capabilities,
    name: Option<NameSource>,
) -> Result<(), String> {
    let mut config = mensor_body_host::Config::new(dial, capabilities);
    config.name = name;
    let (handle, task) = mensor_body_host::start(core, config);

    let mut hangup =
        signal(SignalKind::hangup()).map_err(|error| format!("SIGHUP cannot be heard: {error}"))?;
    tokio::pin!(task);
    loop {
        tokio::select! {
            ended = &mut task => {
                return match ended {
                    Ok(ended) => ended.map_err(|error| error.to_string()),
                    Err(error) => Err(format!("the body stopped: {error}")),
                };
            }
            _ = hangup.recv() => handle.redial(),
            () = stopped() => handle.stop(),
        }
    }
}

/// A name source that reads a file each time it is asked.
fn name_source(path: PathBuf) -> NameSource {
    Arc::new(move || match std::fs::read_to_string(&path) {
        Ok(text) => match Name::new(text.trim()) {
            Ok(name) => Some(name),
            Err(error) => {
                tracing::warn!(file = %path.display(), %error, "the name file holds no usable name");
                None
            }
        },
        Err(error) => {
            tracing::warn!(file = %path.display(), %error, "the name file could not be read");
            None
        }
    })
}

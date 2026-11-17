//! Hosting minds: the flags, and the host they describe.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Args;
use mensor_mind::{MemoryStore, Settings, Store};
use mensor_mind_host::{Config, Host, Provider};
use mensor_store_sqlite::SqliteStore;
use tokio::net::TcpListener;

use crate::{TOKEN, stopped, token};

/// How minds are hosted.
#[derive(Args)]
pub struct Hosting {
    /// The address to listen on. Anything but loopback needs `MENSOR_TOKEN`.
    #[arg(long, default_value = "127.0.0.1:7878")]
    listen: SocketAddr,
    /// The database file that keeps every history. Without one, nothing
    /// outlives this process.
    #[arg(long)]
    store: Option<PathBuf>,
    /// How many clones a mind may have made with `clone_hand` and not let
    /// go. Hands that join by themselves are not counted.
    #[arg(long, default_value_t = 4)]
    max_clones: usize,
    /// How many minds a mind that starts from nothing may fork, its
    /// children's forks included. Zero turns forking off.
    #[arg(long, default_value_t = 64)]
    max_forks: u64,
    /// The model every mind asks. `ANTHROPIC_API_KEY` reaches it through
    /// the Messages API, `OPENAI_API_KEY` through Chat Completions.
    #[arg(long, default_value = "claude-opus-5-5")]
    model: String,
    /// How hard the model works, in its API's words: for Claude low,
    /// medium, high, xhigh or max.
    #[arg(long)]
    effort: Option<String>,
    /// A file holding the system prompt every mind gives its model.
    #[arg(long)]
    system_file: Option<PathBuf>,
    /// Do not let a request Claude declines be served by a fallback model.
    #[arg(long)]
    no_fallbacks: bool,
    /// The tokens one step may read and write before a mind sums its
    /// conversation up and carries on from the summary. Zero never does.
    #[arg(long, default_value_t = 150_000)]
    compact_at: u64,
}

/// Opens a host over its store and model, and the address it serves on.
pub async fn open(hosting: Hosting) -> Result<(Host, TcpListener), String> {
    let token = token();
    if token.is_none() && !hosting.listen.ip().is_loopback() {
        return Err(format!(
            "{} is not loopback: set {TOKEN} so that only its holders reach this host",
            hosting.listen
        ));
    }
    let system = match &hosting.system_file {
        Some(path) => Some(
            std::fs::read_to_string(path)
                .map_err(|error| format!("{} could not be read: {error}", path.display()))?,
        ),
        None => None,
    };
    let store: Arc<dyn Store> = if let Some(path) = &hosting.store {
        Arc::new(SqliteStore::open(path).map_err(|error| error.to_string())?)
    } else {
        tracing::warn!("no --store: histories are kept in memory and end with this process");
        Arc::new(MemoryStore::new())
    };
    let mut model =
        mensor_model::Config::from_vars(&hosting.model, |name| std::env::var(name).ok())
            .map_err(|error| error.to_string())?;
    model.effort = hosting.effort;
    model.fallbacks = !hosting.no_fallbacks;
    let model = Provider::new(model).map_err(|error| error.to_string())?;

    let config = Config {
        token,
        mind: Settings {
            system,
            compact_at: (hosting.compact_at > 0).then_some(hosting.compact_at),
            max_clones: hosting.max_clones,
            max_forks: hosting.max_forks,
            ..Settings::default()
        },
        ..Config::default()
    };
    let host = Host::open(store, Arc::new(model), config)
        .await
        .map_err(|error| error.to_string())?;
    let listener = TcpListener::bind(hosting.listen)
        .await
        .map_err(|error| format!("{} could not be listened on: {error}", hosting.listen))?;
    tracing::info!(listen = %hosting.listen, model = %hosting.model, "serving hands and the API");
    tokio::spawn(async {
        let mut every = tokio::time::interval(TRIM_EVERY);
        loop {
            every.tick().await;
            trim();
        }
    });
    Ok((host, listener))
}

/// How often the memory the host has freed is given back to the system.
const TRIM_EVERY: Duration = Duration::from_secs(30);

/// Gives the memory this process has freed back to the system.
///
/// glibc keeps what a program frees, to hand it out again, and returns it
/// only from the top of its heap. A host frees a great deal at once, when
/// minds rest and when a mind sheds what a summary replaced, and something
/// newer is always above it. So a host would hold its peak for ever unless
/// it asked. Asking takes well under a millisecond.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn trim() {
    #[expect(
        unsafe_code,
        reason = "glibc's malloc_trim is reached only by declaring it, and has no safe wrapper"
    )]
    unsafe extern "C" {
        /// `malloc_trim(3)`, which any thread may call at any time.
        safe fn malloc_trim(pad: usize) -> std::ffi::c_int;
    }
    malloc_trim(0);
}

/// Other allocators return memory by themselves, or cannot be asked.
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn trim() {}

/// Hosts minds until the process is asked to stop.
pub async fn serve(hosting: Hosting) -> Result<(), String> {
    let (host, listener) = open(hosting).await?;
    // Hands hold their connections open, so the host does not wait for
    // them to finish: every history is already stored.
    tokio::select! {
        served = host.serve(listener, std::future::pending()) => {
            served.map_err(|error| format!("the host stopped: {error}"))
        }
        () = stopped() => Ok(()),
    }
}

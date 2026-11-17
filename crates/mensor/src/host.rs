//! Hosting minds: the flags, and the host they describe.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::Args;
use mensor_mind::{MemoryStore, MindSettings, MindTool, NoMindTool, Settings, Store};
use mensor_mind_host::{Config, Host, Provider, Token};
use mensor_store_sqlite::SqliteStore;
use tokio::net::TcpListener;

use crate::{POSTGRES, TOKEN, stopped, token};

/// How minds are hosted.
#[derive(Args)]
pub struct Hosting {
    /// The address to listen on. Anything but loopback needs `MENSOR_TOKEN`.
    #[arg(long, default_value = "127.0.0.1:7878")]
    listen: SocketAddr,
    /// The `SQLite` file that keeps every history. `MENSOR_POSTGRES` names a
    /// Postgres database instead. Without either, nothing outlives this
    /// process.
    #[arg(long)]
    store: Option<PathBuf>,
    /// How many clones a mind may have made with `clone_hand` and not let
    /// go. Hands that join by themselves are not counted.
    #[arg(long, default_value_t = 4)]
    max_clones: usize,
    /// How many minds a mind that starts from nothing may fork, its
    /// children's forks included. Zero turns forking off, and `unlimited`
    /// sets no limit: a hand's `clone` then decides whether a fork is made.
    #[arg(long, default_value = "64", value_parser = forks)]
    max_forks: Forks,
    /// A comma list of the mind's own tools a model is offered, from
    /// `fork`, `clone_hand`, `release_hand`, `tell` and `wait`; `''` offers
    /// none. Each is still shown only while it applies. `POST
    /// /v1/minds/{m}/release` lets a hand go whatever this says.
    #[arg(long, default_value = "fork,clone_hand,release_hand,tell,wait", value_parser = tools)]
    mind_tools: Tools,
    /// The model every mind asks, unless it was made with its own.
    /// `ANTHROPIC_API_KEY` reaches it through the Messages API,
    /// `OPENAI_API_KEY` through Chat Completions, or the Responses API
    /// with `MENSOR_API=responses`.
    #[arg(long, default_value = "claude-opus-5-5")]
    model: String,
    /// A comma list of more models a mind may be made with, beside
    /// `--model`, through the same API and credential.
    #[arg(long, value_delimiter = ',', value_parser = model_name)]
    models: Vec<String>,
    /// The most tokens one reply may hold, at least 16. A reply cut off
    /// while it asks for a tool is asked for again, with that tool not run;
    /// one cut off in its text ends the turn. A model's reasoning counts in
    /// it. `none` sends no cap, for a server that refuses one: Chat
    /// Completions and the Responses API only.
    #[arg(long, default_value = "16000", value_parser = cap)]
    max_tokens: Cap,
    /// How hard the model works, in its API's words: for Claude low,
    /// medium, high, xhigh or max.
    #[arg(long)]
    effort: Option<String>,
    /// A file holding the system prompt a mind gives its model, unless it
    /// was made with its own.
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

/// A limit to forks, or none.
#[derive(Clone, Copy, Debug)]
struct Forks(Option<u64>);

/// `unlimited`, or a number.
fn forks(said: &str) -> Result<Forks, String> {
    if said == "unlimited" {
        return Ok(Forks(None));
    }
    said.parse()
        .map(|limit| Forks(Some(limit)))
        .map_err(|_| format!("{said} is neither a number nor unlimited"))
}

/// A cap on a reply's tokens, or none.
#[derive(Clone, Copy, Debug)]
struct Cap(Option<u32>);

/// `none`, or a number of at least 16, the least the APIs take.
fn cap(said: &str) -> Result<Cap, String> {
    if said == "none" {
        return Ok(Cap(None));
    }
    said.parse()
        .ok()
        .filter(|tokens| *tokens >= 16)
        .map(|tokens| Cap(Some(tokens)))
        .ok_or_else(|| format!("{said} is neither a number of at least 16 nor none"))
}

/// A model's name, which is not empty.
fn model_name(said: &str) -> Result<String, String> {
    let said = said.trim();
    if said.is_empty() {
        return Err("a model's name is not empty".into());
    }
    Ok(said.to_owned())
}

/// Some of the mind's own tools.
#[derive(Clone, Debug)]
struct Tools(Vec<MindTool>);

/// The tools named, each once, split at commas. None is none of them.
fn tools(said: &str) -> Result<Tools, String> {
    let mut tools = Vec::new();
    for name in said
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        let tool: MindTool = name
            .parse()
            .map_err(|error: NoMindTool| error.to_string())?;
        if !tools.contains(&tool) {
            tools.push(tool);
        }
    }
    Ok(Tools(tools))
}

/// Opens a host over its store and model, and the address it serves on.
pub async fn open(hosting: Hosting) -> Result<(Host, TcpListener), String> {
    let token = token().and_then(Token::new);
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
    let store = store(hosting.store.as_deref()).await?;
    let mut model =
        mensor_model::Config::from_vars(&hosting.model, |name| std::env::var(name).ok())
            .map_err(|error| error.to_string())?;
    if let Some(effort) = &hosting.effort {
        let efforts = model.api.efforts();
        if effort.is_empty() || efforts.is_some_and(|known| !known.contains(&effort.as_str())) {
            let known = efforts.map_or_else(|| "any not empty".into(), |known| known.join(", "));
            return Err(format!(
                "--effort {effort} is not one the model's API takes: {known}"
            ));
        }
    }
    if hosting.max_tokens.0.is_none() && model.api == mensor_model::Api::Messages {
        return Err("--max-tokens none is not one the Messages API takes: it needs a cap".into());
    }
    model.effort = hosting.effort;
    model.fallbacks = !hosting.no_fallbacks;
    model.max_tokens = hosting.max_tokens.0;
    let model = Provider::new(model).map_err(|error| error.to_string())?;

    let config = Config {
        token,
        mind: Settings {
            own: MindSettings {
                system,
                ..MindSettings::default()
            },
            compact_at: (hosting.compact_at > 0).then_some(hosting.compact_at),
            max_clones: hosting.max_clones,
            max_forks: hosting.max_forks.0,
            tools: hosting.mind_tools.0,
            ..Settings::default()
        },
        models: hosting.models,
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

/// Where every history is kept: the `SQLite` file `path`, the Postgres
/// database `MENSOR_POSTGRES` names, or memory. One place, never two.
async fn store(path: Option<&Path>) -> Result<Arc<dyn Store>, String> {
    let postgres = std::env::var(POSTGRES).ok().filter(|url| !url.is_empty());
    match (path, postgres) {
        (Some(_), Some(_)) => Err(format!(
            "--store and {POSTGRES} are both set: keep every history in one place"
        )),
        (Some(path), None) => Ok(Arc::new(
            SqliteStore::open(path).map_err(|error| error.to_string())?,
        )),
        (None, Some(url)) => postgres_store(url).await,
        (None, None) => {
            tracing::warn!(
                "no --store and no {POSTGRES}: histories are kept in memory and end with this process"
            );
            Ok(Arc::new(MemoryStore::new()))
        }
    }
}

/// The Postgres database `url` names. Connecting blocks, so it is done off
/// the runtime's threads.
#[cfg(feature = "postgres")]
async fn postgres_store(url: String) -> Result<Arc<dyn Store>, String> {
    let opened =
        tokio::task::spawn_blocking(move || mensor_store_postgres::PostgresStore::open(&url))
            .await
            .map_err(|error| format!("the store could not be opened: {error}"))?;
    Ok(Arc::new(opened.map_err(|error| error.to_string())?))
}

/// No Postgres in this build.
#[cfg(not(feature = "postgres"))]
async fn postgres_store(_: String) -> Result<Arc<dyn Store>, String> {
    Err(format!(
        "{POSTGRES} is set, but this mensor was built without Postgres: build it with \
         --features postgres"
    ))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(clap::Parser)]
    struct Cli {
        #[command(flatten)]
        hosting: Hosting,
    }

    fn parsed(flags: &[&str]) -> Result<Hosting, clap::Error> {
        use clap::Parser;
        let args = std::iter::once("mensor").chain(flags.iter().copied());
        Cli::try_parse_from(args).map(|cli| cli.hosting)
    }

    #[test]
    fn host_flags_read_as_the_readme_says_and_refuse_the_rest() {
        assert_eq!(forks("64").unwrap().0, Some(64));
        assert_eq!(forks("unlimited").unwrap().0, None);
        assert!(forks("lots").is_err());
        assert_eq!(
            parsed(&["--max-tokens", "16"]).unwrap().max_tokens.0,
            Some(16)
        );
        assert_eq!(
            parsed(&["--max-tokens", "none"]).unwrap().max_tokens.0,
            None
        );
        assert!(parsed(&["--max-tokens", "15"]).is_err());
        let offered = parsed(&["--models", "claude-haiku-5-5, claude-sonnet-5-5"]).unwrap();
        assert_eq!(offered.models, ["claude-haiku-5-5", "claude-sonnet-5-5"]);
        assert!(parsed(&["--models", "claude-haiku-5-5,,b"]).is_err());
        assert_eq!(
            tools("tell, fork,tell").unwrap().0,
            [MindTool::Tell, MindTool::Fork]
        );
        assert!(tools("").unwrap().0.is_empty());
        assert!(
            parsed(&["--mind-tools", ""])
                .unwrap()
                .mind_tools
                .0
                .is_empty()
        );
        assert!(tools("fork,bash").unwrap_err().contains("bash"));
    }
}

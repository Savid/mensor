//! Hosting minds: the flags, and the host they describe.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::Args;
use mensor_mind::{MemoryStore, MindSettings, MindTool, NoMindTool, Settings, Store};
use mensor_mind_host::{Config, Host, Model, Provider, Token};
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
    /// The model every mind asks, unless it was made with its own. A name
    /// may start with the API it is asked through: `messages:`, `chat:` or
    /// `responses:`. Without one it is the Messages API if
    /// `ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN` is set, else Chat
    /// Completions with `OPENAI_API_KEY`.
    #[arg(long, default_value = "claude-opus-5-5")]
    model: String,
    /// A comma list of more models a mind may be made with, beside
    /// `--model`, each named as `--model` is. A mind made from another's
    /// history may ask a model behind another API.
    #[arg(long, value_delimiter = ',', value_parser = model_name)]
    models: Vec<String>,
    /// The most tokens one reply through the Messages API may hold, at
    /// least 16: that API needs a cap, and the others are sent none. A reply
    /// cut off while it asks for a tool is asked for again, with that tool
    /// not run; one cut off in its text ends the turn. A model's reasoning
    /// counts in it.
    #[arg(long, default_value_t = 16_000, value_parser = clap::value_parser!(u32).range(16..))]
    max_tokens: u32,
    /// How hard `--model`, and the models behind its API, work unless a
    /// mind says, in that API's words: for Claude low, medium, high, xhigh
    /// or max.
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
    let (model, models) = models(&hosting, |name| std::env::var(name).ok())?;
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
        models,
        ..Config::default()
    };
    let host = Host::open(store, model, config)
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

/// The models a host offers beside its own, by name.
type Offered = BTreeMap<String, Arc<dyn Model>>;

/// The host's own model, and every other it offers, each asked with the
/// variables `var` gives. Models behind one API share a client.
fn models(
    hosting: &Hosting,
    var: impl Fn(&str) -> Option<String>,
) -> Result<(Arc<dyn Model>, Offered), String> {
    let own =
        mensor_model::Config::from_vars(&hosting.model, &var).map_err(|error| error.to_string())?;
    if let Some(effort) = &hosting.effort {
        let efforts = own.api.efforts();
        if effort.is_empty() || efforts.is_some_and(|known| !known.contains(&effort.as_str())) {
            let known = efforts.map_or_else(|| "any not empty".into(), |known| known.join(", "));
            return Err(format!(
                "--effort {effort} is not one the model's API takes: {known}"
            ));
        }
    }
    let mut clients: Vec<(mensor_model::Api, Arc<dyn Model>)> = Vec::new();
    let mut client = |mut config: mensor_model::Config, effort: Option<String>| {
        if let Some((_, model)) = clients.iter().find(|(api, _)| *api == config.api) {
            return Ok(Arc::clone(model));
        }
        config.effort = effort;
        config.fallbacks = !hosting.no_fallbacks;
        config.max_tokens = hosting.max_tokens;
        let api = config.api;
        let model: Arc<dyn Model> =
            Arc::new(Provider::new(config).map_err(|error| error.to_string())?);
        clients.push((api, Arc::clone(&model)));
        Ok::<_, String>(model)
    };
    let own_name = own.model.clone();
    let model = client(own, hosting.effort.clone())?;
    let mut models = BTreeMap::new();
    for named in &hosting.models {
        let config =
            mensor_model::Config::from_vars(named, &var).map_err(|error| error.to_string())?;
        let name = config.model.clone();
        if name == own_name || models.contains_key(&name) {
            return Err(format!("{name} is offered twice"));
        }
        models.insert(name, client(config, None)?);
    }
    Ok((model, models))
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
        assert_eq!(parsed(&["--max-tokens", "16"]).unwrap().max_tokens, 16);
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

    #[test]
    fn each_model_is_asked_through_the_api_its_name_says() {
        let keys = |name: &str| match name {
            "ANTHROPIC_API_KEY" => Some("sk-claude".to_owned()),
            "OPENAI_API_KEY" => Some("sk-openai".to_owned()),
            _ => None,
        };
        let hosting = parsed(&[
            "--model",
            "claude-opus-5-5",
            "--models",
            "claude-haiku-5-5,responses:gpt-6.2,chat:gpt-6.1",
        ])
        .unwrap();
        let (own, offered) = models(&hosting, keys).unwrap();
        let names: Vec<&str> = offered.keys().map(String::as_str).collect();
        assert_eq!(names, ["claude-haiku-5-5", "gpt-6.1", "gpt-6.2"]);
        // Models behind one API share its client; the others take any effort.
        assert!(Arc::ptr_eq(&offered["claude-haiku-5-5"], &own));
        assert!(!Arc::ptr_eq(&offered["gpt-6.1"], &offered["gpt-6.2"]));
        assert!(own.efforts().is_some());
        assert_eq!(offered["gpt-6.2"].efforts(), None);

        let twice = parsed(&["--models", "claude-haiku-5-5,messages:claude-haiku-5-5"]);
        let error = models(&twice.unwrap(), keys).unwrap_err();
        assert!(
            error.contains("claude-haiku-5-5 is offered twice"),
            "{error}"
        );
        let claude_only = |name: &str| keys(name).filter(|_| name.starts_with("ANTHROPIC"));
        let gpt = parsed(&["--models", "responses:gpt-6.2"]).unwrap();
        let error = models(&gpt, claude_only).unwrap_err();
        assert!(error.contains("OPENAI_API_KEY"), "{error}");
    }
}

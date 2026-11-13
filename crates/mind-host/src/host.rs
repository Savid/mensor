use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use mensor_api::{PAGE, PairStatus, Prompted};
use mensor_mind::{
    Conflict, Cut, Fork, ForkError, Forking, Item, Mind, Origin, Pair, Settings, Store, StoreError,
};
use mensor_protocol::{FromBody, Hello, InvalidName, Name, Refusal, VERSION};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};

use crate::model::Model;
use crate::pair::{self, Command, Link, Outgoing, Start, Told, blocking};

/// How a host runs.
#[derive(Clone, Debug)]
pub struct Config {
    /// The bearer token every request must carry. Without one the host
    /// trusts whoever can reach it, so it must listen on loopback only.
    pub token: Option<String>,
    /// The most clones one lineage may have. A clone past it is refused, and
    /// zero turns forking off.
    pub max_clones: usize,
    /// What every mind is told when it is made.
    pub mind: Settings,
    /// How long a new connection has to say hello.
    pub hello_timeout: Duration,
    /// How long a connection may stay silent before it is dropped. A body
    /// pings more often than this.
    pub idle_timeout: Duration,
    /// How long a pair stays in memory with no body connected, nobody
    /// following it and no model asked. After that only its store holds
    /// it, until something concerns it again.
    pub rest_after: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            token: None,
            max_clones: 4,
            mind: Settings::default(),
            hello_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_mins(1),
            rest_after: Duration::from_mins(1),
        }
    }
}

/// A server that hosts minds. Cloning a host gives another handle to the
/// same one.
#[derive(Clone)]
pub struct Host {
    inner: Arc<Inner>,
}

pub(crate) struct Inner {
    pub(crate) store: Arc<dyn Store>,
    pub(crate) model: Arc<dyn Model>,
    pub(crate) config: Config,
    /// Every pair the store holds. A way to reach a pair's task is taken
    /// from here only while this is locked.
    pub(crate) pairs: Mutex<BTreeMap<Name, Slot>>,
    links: AtomicU64,
}

/// One pair, as its host knows it.
pub(crate) struct Slot {
    pair: Pair,
    /// The way to reach the pair's task while the pair is awake. A pair
    /// that rests is in the store alone, and has no task and no mind.
    pub(crate) awake: Option<mpsc::UnboundedSender<Command>>,
}

/// Why the host could not do what was asked.
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    /// No pair has this name.
    #[error("no pair is named {0}")]
    Missing(Name),
    /// The prompt's key is already used by another prompt.
    #[error(transparent)]
    Conflict(#[from] Conflict),
    /// The pair's task has stopped, because its history could not be stored.
    #[error("the pair {0} has stopped")]
    Stopped(Name),
    /// The store failed.
    #[error(transparent)]
    Store(StoreError),
    /// The name asked for is not one a pair can have.
    #[error(transparent)]
    Name(#[from] InvalidName),
}

/// A body's connection, accepted.
struct Attached {
    commands: mpsc::UnboundedSender<Command>,
    link: u64,
}

/// Why a body was not connected.
#[derive(Debug, thiserror::Error)]
pub enum Rejected {
    /// The host turned the body away.
    #[error("the host refused the body: {}", .0.reason())]
    Refused(Refusal),
    /// The host failed.
    #[error("the host could not attach the body: {0}")]
    Failed(String),
}

/// A body's connection to its mind, whatever carries it: a WebSocket, or
/// nothing at all when the body runs in the host's own process. Dropping it
/// detaches the body.
pub struct Connection {
    commands: mpsc::UnboundedSender<Command>,
    link: u64,
    outgoing: mpsc::UnboundedReceiver<Outgoing>,
}

impl Connection {
    /// Hands the mind a message from the body. `false` once the pair has
    /// stopped.
    pub fn send(&self, message: FromBody) -> bool {
        let command = Command::FromBody {
            link: self.link,
            message,
        };
        self.commands.send(command).is_ok()
    }

    /// What the host has for the body next, or `None` once the pair has
    /// stopped. A caller may drop this future and call again without losing
    /// a message.
    pub async fn next(&mut self) -> Option<Outgoing> {
        self.outgoing.recv().await
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Detached { link: self.link });
    }
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("link", &self.link)
            .finish_non_exhaustive()
    }
}

impl Host {
    /// Opens a host over a store. Every pair the store holds rests until
    /// its body dials in or something is asked of it.
    pub fn open(
        store: Arc<dyn Store>,
        model: Arc<dyn Model>,
        config: Config,
    ) -> Result<Self, StoreError> {
        let pairs: BTreeMap<Name, Slot> = store
            .pairs()?
            .into_iter()
            .map(|pair| (pair.name.clone(), Slot { pair, awake: None }))
            .collect();
        tracing::info!(pairs = pairs.len(), "the host is open");
        let inner = Inner {
            store,
            model,
            config,
            pairs: Mutex::new(pairs),
            links: AtomicU64::new(1),
        };
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// The host's configuration.
    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    /// Serves bodies and the API on `listener` until `shutdown` resolves.
    pub async fn serve(
        self,
        listener: TcpListener,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> std::io::Result<()> {
        axum::serve(listener, self.router())
            .with_graceful_shutdown(shutdown)
            .await
    }

    /// How many pairs are awake: in memory, each with a mind and a task.
    /// Every other pair the host knows is in its store alone.
    pub async fn awake(&self) -> usize {
        let pairs = self.inner.pairs.lock().await;
        pairs.values().filter(|slot| slot.awake.is_some()).count()
    }

    /// Every pair, in the order of their names.
    pub async fn pairs(&self) -> Vec<PairStatus> {
        let known: Vec<Known> = {
            let pairs = self.inner.pairs.lock().await;
            pairs.values().map(Known::of).collect()
        };
        let mut statuses = Vec::with_capacity(known.len());
        for known in known {
            if let Ok(status) = self.status(known).await {
                statuses.push(status);
            }
        }
        statuses
    }

    /// One pair.
    pub async fn pair(&self, name: &Name) -> Result<PairStatus, HostError> {
        let known = {
            let pairs = self.inner.pairs.lock().await;
            let slot = pairs.get(name);
            Known::of(slot.ok_or_else(|| HostError::Missing(name.clone()))?)
        };
        self.status(known).await
    }

    /// What a pair is doing. A pair that is awake says. One that rests is
    /// read from the store, and stays at rest.
    async fn status(&self, known: Known) -> Result<PairStatus, HostError> {
        let Known { pair, awake } = known;
        let stopped = || HostError::Stopped(pair.name.clone());
        if let Some(commands) = awake {
            let (reply, answer) = oneshot::channel();
            commands
                .send(Command::Status { reply })
                .map_err(|_| stopped())?;
            let status = answer.await.map_err(|_| stopped())?;
            return Ok(PairStatus {
                pair,
                state: status.state,
                attached: status.attached,
                length: status.length,
            });
        }
        // The latest of its history says what a pair is doing: far less
        // than its mind would be loaded with.
        let store = Arc::clone(&self.inner.store);
        let read = blocking(move || {
            let (from, items) = store.recent(&pair.name)?;
            let (state, length) = Mind::status(from, items);
            Ok(PairStatus {
                pair,
                state,
                attached: false,
                length,
            })
        });
        read.await.map_err(HostError::Store)
    }

    /// Says something to a pair's mind. The prompt is stored when this
    /// returns.
    pub async fn prompt(
        &self,
        name: &Name,
        key: Option<String>,
        text: String,
    ) -> Result<Prompted, HostError> {
        let commands = self.reach(name).await?;
        let (reply, answer) = oneshot::channel();
        let stopped = || HostError::Stopped(name.clone());
        commands
            .send(Command::Prompt { key, text, reply })
            .map_err(|_| stopped())?;
        drop(commands);
        match answer.await.map_err(|_| stopped())? {
            Ok(true) => Ok(Prompted::Existing),
            Ok(false) => Ok(Prompted::Accepted),
            Err(conflict) => Err(conflict.into()),
        }
    }

    /// Stops a pair's turn: the model's reply is given up and the body is
    /// told to cancel what it runs. The pair is idle once the body has
    /// answered for those calls, which the status returned may not show yet.
    pub async fn interrupt(&self, name: &Name) -> Result<PairStatus, HostError> {
        let commands = self.reach(name).await?;
        commands
            .send(Command::Interrupt)
            .map_err(|_| HostError::Stopped(name.clone()))?;
        drop(commands);
        self.pair(name).await
    }

    /// A pair's history from index `from`, [`PAGE`] items of it at the
    /// most. This reads the store on the calling thread, so an async
    /// caller runs it where blocking is allowed.
    pub fn history(&self, name: &Name, from: u64) -> Result<Vec<Item>, HostError> {
        let read = self.inner.store.read(name, from, PAGE);
        read.map_err(|error| match error {
            StoreError::Missing(name) => HostError::Missing(name),
            other => HostError::Store(other),
        })
    }

    /// What a pair tells from now on: that its history grew, and what its
    /// model writes. The pair stays awake for as long as it is followed.
    pub(crate) async fn follow(&self, name: &Name) -> Result<broadcast::Receiver<Told>, HostError> {
        let commands = self.reach(name).await?;
        let (reply, answer) = oneshot::channel();
        let stopped = || HostError::Stopped(name.clone());
        commands
            .send(Command::Follow { reply })
            .map_err(|_| stopped())?;
        drop(commands);
        answer.await.map_err(|_| stopped())
    }

    /// The way to reach a pair's task, waking the pair if it rests.
    async fn reach(&self, name: &Name) -> Result<mpsc::UnboundedSender<Command>, HostError> {
        let mut pairs = self.inner.pairs.lock().await;
        let slot = pairs.get_mut(name);
        let slot = slot.ok_or_else(|| HostError::Missing(name.clone()))?;
        Ok(self.wake(slot))
    }

    fn wake(&self, slot: &mut Slot) -> mpsc::UnboundedSender<Command> {
        let awake = slot
            .awake
            .get_or_insert_with(|| pair::spawn(&slot.pair, Start::Stored, &self.inner));
        awake.clone()
    }

    /// Connects a body that has said `hello`, over whatever carries it.
    pub async fn connect(&self, hello: Hello) -> Result<Connection, Rejected> {
        if hello.v != VERSION {
            return Err(Rejected::Refused(Refusal::Version));
        }
        let name = hello.name.clone();
        let (outbound, outgoing) = mpsc::unbounded_channel();
        match self.attach(hello, outbound).await {
            Ok(Attached { commands, link }) => Ok(Connection {
                commands,
                link,
                outgoing,
            }),
            Err(rejected) => {
                match &rejected {
                    Rejected::Refused(refusal) => {
                        tracing::info!(body = %name, reason = refusal.reason(), "a body was refused");
                    }
                    Rejected::Failed(error) => {
                        tracing::error!(body = %name, %error, "a body could not be attached");
                    }
                }
                Err(rejected)
            }
        }
    }

    /// Takes a body's `hello` and joins the connection to a mind.
    ///
    /// A name the host knows is that pair's body reconnecting. A new name
    /// with no `was` starts a pair. A new name with a `was` is a clone: the
    /// mind of the body it names forks, unless the lineage has all the
    /// clones it may.
    async fn attach(
        &self,
        hello: Hello,
        outbound: mpsc::UnboundedSender<Outgoing>,
    ) -> Result<Attached, Rejected> {
        let inner = &self.inner;
        let link = Link {
            id: inner.links.fetch_add(1, Ordering::Relaxed),
            outbound,
        };
        let id = link.id;
        let mut pairs = inner.pairs.lock().await;

        if let Some(slot) = pairs.get_mut(&hello.name) {
            let commands = self.wake(slot);
            let stopped = || Rejected::Failed(format!("the pair {} has stopped", hello.name));
            commands
                .send(Command::Attach {
                    hello: Box::new(hello.clone()),
                    link,
                })
                .map_err(|_| stopped())?;
            return Ok(Attached { commands, link: id });
        }

        let name = hello.name.clone();
        let internal = |error: StoreError| Rejected::Failed(error.to_string());
        let (pair, commands) = match hello.was.clone() {
            None => {
                let pair = Pair {
                    name: name.clone(),
                    origin: Origin::Root,
                };
                let store = Arc::clone(&inner.store);
                let made = pair.clone();
                blocking(move || store.create(&made))
                    .await
                    .map_err(internal)?;
                tracing::info!(pair = %name, "a new pair");
                let commands = pair::spawn(&pair, Start::Stored, inner);
                let _ = commands.send(Command::Attach {
                    hello: Box::new(hello),
                    link,
                });
                (pair, commands)
            }
            Some(parent) => {
                if !pairs.contains_key(&parent) {
                    return Err(Rejected::Refused(Refusal::UnknownParent));
                }
                if clones_of(&pairs, &root_of(&pairs, &parent)) >= inner.config.max_clones {
                    return Err(Rejected::Refused(Refusal::Limit));
                }
                // A fork is worked out from all its parent has stored, which
                // is all the parent has done: nothing is sent before it is.
                let (store, settings) = (Arc::clone(&inner.store), inner.config.mind.clone());
                let (from, clone) = (parent.clone(), hello.clone());
                let forked = blocking(move || fork(&*store, &from, settings, &clone));
                let fork = match forked.await.map_err(internal)? {
                    Ok(fork) => fork,
                    Err(error @ ForkError::UnknownCall(_)) => {
                        tracing::warn!(pair = %name, %parent, %error, "not a clone");
                        return Err(Rejected::Refused(Refusal::Stranger));
                    }
                };
                let origin = Origin::Fork {
                    parent: parent.clone(),
                    inherited: fork.inherited,
                };
                let pair = Pair {
                    name: name.clone(),
                    origin,
                };
                let store = Arc::clone(&inner.store);
                let made = pair.clone();
                blocking(move || store.create(&made))
                    .await
                    .map_err(internal)?;
                tracing::info!(pair = %name, %parent, inherited = fork.inherited, "a clone attached: the mind forked");
                let start = Start::Forked {
                    mind: Box::new(fork.mind),
                    first: fork.outputs,
                    link,
                };
                (pair.clone(), pair::spawn(&pair, start, inner))
            }
        };
        let awake = Some(commands.clone());
        pairs.insert(name, Slot { pair, awake });
        Ok(Attached { commands, link: id })
    }
}

/// Makes the mind for a clone of `parent`'s body.
///
/// The parent's history is read a page at a time and never held whole:
/// once through to find where the two part, then the end of what they
/// share, and then on from the parting for as long as the clone has lost
/// calls whose outcome the parent's history may hold.
fn fork(
    store: &dyn Store,
    parent: &Name,
    settings: Settings,
    hello: &Hello,
) -> Result<Result<Fork, ForkError>, StoreError> {
    let length = |items: &[Item]| u64::try_from(items.len()).unwrap_or(u64::MAX);
    let mut cut = Cut::new(hello);
    let mut at = 0;
    loop {
        let page = store.read(parent, at, PAGE)?;
        if page.is_empty() {
            break;
        }
        cut.read(&page);
        at += length(&page);
    }
    let parting = match cut.end() {
        Ok(parting) => parting,
        Err(error) => return Ok(Err(error)),
    };
    let shared = usize::try_from(parting.cut - parting.from).unwrap_or(usize::MAX);
    let shared = store.read(parent, parting.from, shared)?;
    let mut forking = Forking::new(parting, shared, settings, hello);
    let mut at = parting.cut;
    while forking.wants() {
        let page = store.read(parent, at, PAGE)?;
        if page.is_empty() {
            break;
        }
        forking.later(&page);
        at += length(&page);
    }
    Ok(Ok(forking.end()))
}

/// A pair and the way to reach it, taken from the host's map.
struct Known {
    pair: Pair,
    awake: Option<mpsc::UnboundedSender<Command>>,
}

impl Known {
    fn of(slot: &Slot) -> Self {
        Self {
            pair: slot.pair.clone(),
            awake: slot.awake.clone(),
        }
    }
}

/// The pair a lineage began with.
fn root_of(pairs: &BTreeMap<Name, Slot>, name: &Name) -> Name {
    let mut name = name;
    while let Some(Origin::Fork { parent, .. }) = pairs.get(name).map(|slot| &slot.pair.origin) {
        name = parent;
    }
    name.clone()
}

/// How many clones the lineage of `root` has.
fn clones_of(pairs: &BTreeMap<Name, Slot>, root: &Name) -> usize {
    pairs
        .keys()
        .filter(|name| *name != root && root_of(pairs, name) == *root)
        .count()
}

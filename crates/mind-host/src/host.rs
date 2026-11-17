use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use mensor_api::{MindStatus, PAGE, Prompted, Token};
use mensor_mind::{
    Conflict, Item, Mind, MindRecord, NoHand, Origin, Report, Settings, State, Store, StoreError,
};
use mensor_protocol::{FromHand, Hello, InvalidName, Name, Refusal};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, broadcast, mpsc, oneshot, watch};

use crate::model::Model;
use crate::task::{self, Command, Link, Outgoing, Told, blocking};

/// How a host runs.
#[derive(Clone, Debug)]
pub struct Config {
    /// The bearer token every request must carry, or a token made from it
    /// for the mind asked about. Without one the host trusts every process
    /// on its machine, so it must listen on loopback only, and it serves
    /// only requests from its own machine.
    pub token: Option<Token>,
    /// What every mind is told when it is made: its hands and forks
    /// included.
    pub mind: Settings,
    /// How long a new connection has to say hello.
    pub hello_timeout: Duration,
    /// How long a connection may stay silent before it is dropped. A hand
    /// pings more often than this.
    pub idle_timeout: Duration,
    /// How long a mind stays in memory with no hand connected, nobody
    /// following it and no model asked. After that only its store holds
    /// it, until something concerns it again.
    pub rest_after: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            token: None,
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

impl From<Arc<Inner>> for Host {
    fn from(inner: Arc<Inner>) -> Self {
        Self { inner }
    }
}

pub(crate) struct Inner {
    pub(crate) store: Arc<dyn Store>,
    pub(crate) model: Arc<dyn Model>,
    pub(crate) config: Config,
    /// Every mind the store holds. A way to reach a mind's task is taken
    /// from here only while this is locked.
    pub(crate) minds: Mutex<BTreeMap<Name, Slot>>,
    links: AtomicU64,
    /// The minds a `wait` waits for, each with the minds that wait for it.
    /// Kept in memory alone: a mind that waits watches again when it wakes.
    watchers: std::sync::Mutex<BTreeMap<Name, BTreeSet<Name>>>,
    /// Whether the host has stopped: every mind's task and hand's
    /// connection ends once it has.
    pub(crate) stopped: watch::Sender<bool>,
}

/// One mind, as its host knows it.
pub(crate) struct Slot {
    pub(crate) record: MindRecord,
    /// The way to reach the mind's task while it is awake. A mind that
    /// rests is in the store alone, and has no task.
    pub(crate) awake: Option<mpsc::UnboundedSender<Command>>,
}

/// Why the host could not do what was asked.
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    /// No mind has this name.
    #[error("no mind is named {0}")]
    Missing(Name),
    /// The prompt's key is already used by another prompt.
    #[error(transparent)]
    Conflict(#[from] Conflict),
    /// The mind has no such hand to release.
    #[error(transparent)]
    NoHand(#[from] NoHand),
    /// The mind's task has stopped, because its history could not be stored.
    #[error("the mind {0} has stopped")]
    Stopped(Name),
    /// The store failed.
    #[error(transparent)]
    Store(StoreError),
    /// The name asked for is not one a mind can have.
    #[error(transparent)]
    Name(#[from] InvalidName),
}

/// Why a hand was not connected.
#[derive(Debug, thiserror::Error)]
pub enum Rejected {
    /// The host turned the hand away.
    #[error("the host refused the hand: {}", .0.reason())]
    Refused(Refusal),
    /// The host failed.
    #[error("the host could not attach the hand: {0}")]
    Failed(String),
}

/// A hand's connection to its mind, whatever carries it: a WebSocket, or
/// nothing at all when the hand runs in the host's own process. Dropping it
/// detaches the hand.
pub struct Connection {
    commands: mpsc::UnboundedSender<Command>,
    hand: Name,
    link: u64,
    outgoing: mpsc::UnboundedReceiver<Outgoing>,
}

impl Connection {
    /// Hands the mind a message from the hand. `false` once the mind has
    /// stopped.
    pub fn send(&self, message: FromHand) -> bool {
        let command = Command::FromHand {
            hand: self.hand.clone(),
            link: self.link,
            message,
        };
        self.commands.send(command).is_ok()
    }

    /// What the host has for the hand next, or `None` once the mind has
    /// stopped. A caller may drop this future and call again without losing
    /// a message.
    pub async fn next(&mut self) -> Option<Outgoing> {
        self.outgoing.recv().await
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Detached {
            hand: self.hand.clone(),
            link: self.link,
        });
    }
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("hand", &self.hand)
            .field("link", &self.link)
            .finish_non_exhaustive()
    }
}

impl Host {
    /// Opens a host over a store. A mind with something to do that waits
    /// for no hand is woken soon after: one saying a message, waiting for
    /// its children, or with its model to ask and every hand let go. Every
    /// other mind rests until a hand dials in or something is asked of it.
    pub async fn open(
        store: Arc<dyn Store>,
        model: Arc<dyn Model>,
        config: Config,
    ) -> Result<Self, StoreError> {
        let listed = Arc::clone(&store);
        let minds: BTreeMap<Name, Slot> = blocking(move || listed.minds())
            .await?
            .into_iter()
            .map(|record| {
                let slot = Slot {
                    record: record.clone(),
                    awake: None,
                };
                (record.name, slot)
            })
            .collect();
        tracing::info!(minds = minds.len(), "the host is open");
        let inner = Inner {
            store,
            model,
            config,
            minds: Mutex::new(minds),
            links: AtomicU64::new(1),
            watchers: std::sync::Mutex::default(),
            stopped: watch::Sender::new(false),
        };
        let host = Self {
            inner: Arc::new(inner),
        };
        host.resume();
        Ok(host)
    }

    /// Wakes, in the background, every mind that has something to do and
    /// waits for no hand. A mind whose history cannot be read is left to
    /// rest, and fails when something wakes it.
    fn resume(&self) {
        let host = self.clone();
        tokio::spawn(async move {
            let names: Vec<Name> = host.inner.minds.lock().await.keys().cloned().collect();
            let store = Arc::clone(&host.inner.store);
            let working = blocking(move || {
                let working = names.into_iter().filter(|name| match store.recent(name) {
                    Ok(latest) => {
                        let status = Mind::status(name.clone(), latest);
                        status.state != State::Idle && status.waiting.is_empty()
                    }
                    Err(error) => {
                        tracing::warn!(mind = %name, %error, "a mind's history could not be read");
                        false
                    }
                });
                Ok(working.collect::<Vec<_>>())
            })
            .await
            .unwrap_or_default();
            let mut minds = host.inner.minds.lock().await;
            for name in working {
                if let Some(slot) = minds.get_mut(&name) {
                    tracing::info!(mind = %name, "a mind with work to do is woken");
                    host.wake(slot);
                }
            }
        });
    }

    /// The host's configuration.
    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    /// Serves hands and the API on `listener` until `shutdown` resolves.
    pub async fn serve(
        self,
        listener: TcpListener,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> std::io::Result<()> {
        axum::serve(listener, self.router())
            .with_graceful_shutdown(shutdown)
            .await
    }

    /// Stops the host, as a process that exits would: every mind's task
    /// ends between what it does, and every hand's connection is closed
    /// with 1001, so the hand dials again. What they hold, the store among
    /// it, is let go as each ends. A mind woken after this ends at once.
    pub fn stop(&self) {
        self.inner.stopped.send_replace(true);
    }

    /// Whether the host has stopped, to wait on.
    pub(crate) fn stopping(&self) -> watch::Receiver<bool> {
        self.inner.stopped.subscribe()
    }

    /// How many minds are awake: in memory, each with a task. Every other
    /// mind the host knows is in its store alone.
    pub async fn awake(&self) -> usize {
        let minds = self.inner.minds.lock().await;
        minds.values().filter(|slot| slot.awake.is_some()).count()
    }

    /// Every mind, in the order of their names.
    pub async fn minds(&self) -> Vec<MindStatus> {
        let known: Vec<Known> = {
            let minds = self.inner.minds.lock().await;
            minds.values().map(Known::of).collect()
        };
        let mut statuses = Vec::with_capacity(known.len());
        for known in known {
            if let Ok(status) = self.status(known).await {
                statuses.push(status);
            }
        }
        statuses
    }

    /// One mind.
    pub async fn mind(&self, name: &Name) -> Result<MindStatus, HostError> {
        let known = {
            let minds = self.inner.minds.lock().await;
            let slot = minds.get(name);
            Known::of(slot.ok_or_else(|| HostError::Missing(name.clone()))?)
        };
        self.status(known).await
    }

    /// What a mind is doing. A mind that is awake says. One that rests is
    /// read from the store, and stays at rest: it has no hand connected.
    async fn status(&self, known: Known) -> Result<MindStatus, HostError> {
        let Known { record, awake } = known;
        let stopped = || HostError::Stopped(record.name.clone());
        if let Some(commands) = awake {
            let (reply, answer) = oneshot::channel();
            commands
                .send(Command::Status { reply })
                .map_err(|_| stopped())?;
            let status = answer.await.map_err(|_| stopped())?;
            return Ok(MindStatus {
                mind: record,
                state: status.state,
                hands: status.hands,
                waiting: status.waiting,
                watching: status.watching,
                length: status.length,
            });
        }
        // The latest of its history says what a mind is doing: far less
        // than it would be loaded with.
        let store = Arc::clone(&self.inner.store);
        let read = blocking(move || {
            let status = Mind::status(record.name.clone(), store.recent(&record.name)?);
            Ok(MindStatus {
                mind: record,
                state: status.state,
                hands: Vec::new(),
                waiting: status.waiting,
                watching: status.watching,
                length: status.length,
            })
        });
        read.await.map_err(HostError::Store)
    }

    /// Says something to a mind. The prompt is stored when this returns.
    pub async fn prompt(
        &self,
        name: &Name,
        key: Option<String>,
        text: String,
    ) -> Result<Prompted, HostError> {
        self.say(name, key, text, None).await
    }

    /// Says something to a mind, as `from` when its parent or a child says
    /// it.
    pub(crate) async fn say(
        &self,
        name: &Name,
        key: Option<String>,
        text: String,
        from: Option<Name>,
    ) -> Result<Prompted, HostError> {
        let commands = self.reach(name).await?;
        let (reply, answer) = oneshot::channel();
        let stopped = || HostError::Stopped(name.clone());
        let prompt = Command::Prompt {
            key,
            text,
            from,
            reply,
        };
        commands.send(prompt).map_err(|_| stopped())?;
        drop(commands);
        match answer.await.map_err(|_| stopped())? {
            Ok(true) => Ok(Prompted::Existing),
            Ok(false) => Ok(Prompted::Accepted),
            Err(conflict) => Err(conflict.into()),
        }
    }

    /// Stops a mind's turn: the model's reply is given up and its hands are
    /// told to cancel what they run. The mind is idle once its hands have
    /// answered for those calls, which the status returned may not show yet.
    pub async fn interrupt(&self, name: &Name) -> Result<MindStatus, HostError> {
        let commands = self.reach(name).await?;
        commands
            .send(Command::Interrupt)
            .map_err(|_| HostError::Stopped(name.clone()))?;
        drop(commands);
        self.mind(name).await
    }

    /// Lets one of a mind's hands go, from outside its conversation: the
    /// calls it was sent and never answered are stored as unknown, and the
    /// hand is refused from now on.
    pub async fn release(&self, name: &Name, hand: Name) -> Result<MindStatus, HostError> {
        let commands = self.reach(name).await?;
        let (reply, answer) = oneshot::channel();
        let stopped = || HostError::Stopped(name.clone());
        commands
            .send(Command::Release { hand, reply })
            .map_err(|_| stopped())?;
        drop(commands);
        answer.await.map_err(|_| stopped())??;
        self.mind(name).await
    }

    /// A mind's history from index `from`, [`PAGE`] items of it at the
    /// most. This reads the store on the calling thread, so an async
    /// caller runs it where blocking is allowed.
    pub fn history(&self, name: &Name, from: u64) -> Result<Vec<Item>, HostError> {
        let read = self.inner.store.read(name, from, PAGE);
        read.map_err(|error| match error {
            StoreError::Missing(name) => HostError::Missing(name),
            other => HostError::Store(other),
        })
    }

    /// What a mind tells from now on: that its history grew, and what its
    /// model writes. The mind stays awake for as long as it is followed.
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

    /// The minds waited for, each with the minds that wait.
    fn watchers(&self) -> std::sync::MutexGuard<'_, BTreeMap<Name, BTreeSet<Name>>> {
        let watchers = self.inner.watchers.lock();
        watchers.unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Whether a mind waits for `mind` to rest.
    pub(crate) fn is_watched(&self, mind: &Name) -> bool {
        let watchers = self.watchers();
        watchers.contains_key(mind)
    }

    /// `parent` waits for each of `minds` to rest: each tells it what it
    /// said once it is, at once if it already is.
    pub(crate) fn watch(&self, parent: &Name, minds: Vec<Name>) {
        for mind in minds {
            {
                let mut watchers = self.watchers();
                watchers
                    .entry(mind.clone())
                    .or_default()
                    .insert(parent.clone());
            }
            let host = self.clone();
            tokio::spawn(async move {
                // One that rests from now on tells its watchers itself.
                let report = match host.mind_report(&mind).await {
                    Ok(report) => report,
                    Err(HostError::Missing(_)) => Some(Report::Missing),
                    Err(error) => {
                        tracing::warn!(%mind, %error, "a mind waited for could not be told of");
                        None
                    }
                };
                if let Some(report) = report {
                    host.rested(&mind, &report);
                }
            });
        }
    }

    /// `mind` is at rest, having said `report`: every mind that waits for
    /// it is told.
    pub(crate) fn rested(&self, mind: &Name, report: &Report) {
        let parents = {
            let mut watchers = self.watchers();
            watchers.remove(mind).unwrap_or_default()
        };
        for parent in parents {
            let (host, mind, report) = (self.clone(), mind.clone(), report.clone());
            tokio::spawn(async move {
                match host.reach(&parent).await {
                    Ok(commands) => {
                        let _ = commands.send(Command::Rested { mind, report });
                    }
                    Err(error) => {
                        tracing::warn!(%parent, %error, "a mind that waits could not be told");
                    }
                }
            });
        }
    }

    /// What a mind said when it came to rest, if it is at rest.
    async fn mind_report(&self, name: &Name) -> Result<Option<Report>, HostError> {
        let known = {
            let minds = self.inner.minds.lock().await;
            let slot = minds.get(name);
            Known::of(slot.ok_or_else(|| HostError::Missing(name.clone()))?)
        };
        let stopped = || HostError::Stopped(name.clone());
        if let Some(commands) = known.awake {
            let (reply, answer) = oneshot::channel();
            commands
                .send(Command::Status { reply })
                .map_err(|_| stopped())?;
            return Ok(answer.await.map_err(|_| stopped())?.report);
        }
        let (store, of) = (Arc::clone(&self.inner.store), name.clone());
        let status = blocking(move || Ok(Mind::status(of.clone(), store.recent(&of)?)));
        Ok(status.await.map_err(HostError::Store)?.report)
    }

    /// The way to reach a mind's task, waking the mind if it rests.
    async fn reach(&self, name: &Name) -> Result<mpsc::UnboundedSender<Command>, HostError> {
        let mut minds = self.inner.minds.lock().await;
        let slot = minds.get_mut(name);
        let slot = slot.ok_or_else(|| HostError::Missing(name.clone()))?;
        Ok(self.wake(slot))
    }

    fn wake(&self, slot: &mut Slot) -> mpsc::UnboundedSender<Command> {
        let awake = slot
            .awake
            .get_or_insert_with(|| task::spawn(&slot.record.name, &self.inner));
        awake.clone()
    }

    /// Connects a hand that has said `hello`, over whatever carries it.
    ///
    /// A mind the host knows takes the hand, or refuses it. A name the host
    /// does not know starts a mind, if it is a root name: one with a `.`
    /// is a fork child not made yet, and the hand is told to dial again.
    pub async fn connect(&self, hello: Hello) -> Result<Connection, Rejected> {
        let (mind, hand) = (hello.mind.clone(), hello.hand.clone());
        let (outbound, outgoing) = mpsc::unbounded_channel();
        let link = Link {
            id: self.inner.links.fetch_add(1, Ordering::Relaxed),
            outbound,
        };
        let id = link.id;
        let mut minds = self.inner.minds.lock().await;
        let slot = match minds.entry(mind.clone()) {
            Entry::Occupied(known) => known.into_mut(),
            Entry::Vacant(_) if !mind.is_root() => {
                let refusal = if mind.as_str().contains('.') {
                    Refusal::UnknownMind
                } else {
                    Refusal::Name
                };
                tracing::debug!(%mind, %hand, reason = refusal.reason(), "a hand was refused");
                return Err(Rejected::Refused(refusal));
            }
            Entry::Vacant(new) => {
                let record = MindRecord {
                    name: mind.clone(),
                    origin: Origin::Root,
                };
                let (store, made) = (Arc::clone(&self.inner.store), record.clone());
                blocking(move || store.create(&made, &[]))
                    .await
                    .map_err(|error| Rejected::Failed(error.to_string()))?;
                tracing::info!(%mind, "a new mind");
                new.insert(Slot {
                    record,
                    awake: None,
                })
            }
        };
        let commands = self.wake(slot);
        let attach = Command::Attach {
            hello: Box::new(hello),
            link,
        };
        commands
            .send(attach)
            .map_err(|_| Rejected::Failed(format!("the mind {mind} has stopped")))?;
        Ok(Connection {
            commands,
            hand,
            link: id,
            outgoing,
        })
    }
}

/// A mind and the way to reach it, taken from the host's map.
struct Known {
    record: MindRecord,
    awake: Option<mpsc::UnboundedSender<Command>>,
}

impl Known {
    fn of(slot: &Slot) -> Self {
        Self {
            record: slot.record.clone(),
            awake: slot.awake.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_logged_through_the_log_crate() {
        // A host is built with the hand's crate as often as not, and
        // tungstenite logs a hand's handshake there, `Authorization`
        // included.
        assert_eq!(log::STATIC_MAX_LEVEL, log::LevelFilter::Off);
    }

    #[test]
    fn a_config_never_shows_its_token() {
        let config = Config {
            token: Token::new("fake-host-token"),
            ..Config::default()
        };
        let shown = format!("{config:?}");
        assert!(!shown.contains("fake-host-token"), "{shown}");
        assert!(shown.contains("token: Some(Token(..))"), "{shown}");
    }
}

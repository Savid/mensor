use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use mensor_api::{Made, MindStatus, PAGE, Prompted, Token};
use mensor_mind::{
    Conflict, Item, MadeFrom, Mind, MindRecord, MindSettings, NoHand, Origin, Report, Settings,
    State, Store, StoreError, Unbranched,
};
use mensor_protocol::{FromHand, Hello, InvalidName, Name, Refusal};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, broadcast, mpsc, oneshot, watch};
use tokio::task::AbortHandle;

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
    /// included. A mind made with settings of its own takes those over
    /// these.
    pub mind: Settings,
    /// The models a mind may be made with beside the host's own, which it
    /// may always be, by name, each with the [`Model`] that asks it. One
    /// made with none asks the host's.
    pub models: BTreeMap<String, Arc<dyn Model>>,
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
            models: BTreeMap::new(),
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
    /// The `wait` of each mind that waits, by its name. Kept in memory
    /// alone: a mind that waits watches again when it wakes.
    waits: std::sync::Mutex<BTreeMap<Name, Wait>>,
    /// The settings minds were made with, by their digest, so that minds
    /// made the same way share one.
    shared: std::sync::Mutex<HashMap<String, Weak<MindSettings>>>,
    /// Whether the host has stopped: every mind's task and hand's
    /// connection ends once it has.
    pub(crate) stopped: watch::Sender<bool>,
}

impl Inner {
    /// What asks `model`: the host's own model for its own name, for none,
    /// and for one it no longer offers.
    pub(crate) fn asking(&self, model: Option<&str>) -> &Arc<dyn Model> {
        model
            .and_then(|model| self.config.models.get(model))
            .unwrap_or(&self.model)
    }
}

/// A mind's `wait`, as its host keeps it.
struct Wait {
    /// Which wait it is.
    key: String,
    /// The minds it waits for that have not said they rest.
    unheard: BTreeSet<Name>,
    /// Its timer, which stops when the wait ends.
    timer: AbortHandle,
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
    /// The prompt's key is already used by another prompt, or the mind has
    /// ended.
    #[error(transparent)]
    Conflict(#[from] Conflict),
    /// A mind of this name was made otherwise.
    #[error("a mind named {0} was made otherwise: with another origin or other settings")]
    Exists(Name),
    /// A mind is made with a root name.
    #[error(
        "{0} is no root's name: a mind is made with one that holds no '.', of 55 bytes at most"
    )]
    NotRoot(Name),
    /// The host does not offer the model asked for.
    #[error("this host does not offer the model {asked}: it offers {}", .offered.join(", "))]
    Model {
        /// The model asked for.
        asked: String,
        /// Those it offers.
        offered: Vec<String>,
    },
    /// The host's model's API does not take the effort asked for.
    #[error("{}", effort_refused(.asked, .taken.as_deref()))]
    Effort {
        /// The effort asked for.
        asked: String,
        /// Those the API takes, or none when it takes any that is not empty.
        taken: Option<String>,
    },
    /// A mind cannot be made from those items of the history named.
    #[error(transparent)]
    Unbranched(#[from] Unbranched),
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

/// Why an effort is refused, for a person.
fn effort_refused(asked: &str, taken: Option<&str>) -> String {
    match (asked, taken) {
        ("", _) => "an effort is not empty".into(),
        (asked, Some(taken)) => {
            format!("this host's model does not take the effort {asked}: it takes {taken}")
        }
        (asked, None) => format!("this host's model does not take the effort {asked}"),
    }
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
        mut config: Config,
    ) -> Result<Self, StoreError> {
        // A mind with no model of its own asks the host's, and says so, so
        // that what its reasoning was made under names the model.
        if config.mind.own.model.is_none() {
            config.mind.own.model = Some(model.name().to_owned());
        }
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
        // The store gives minds made the same way one settings: each is
        // shared from here.
        let mut shared = HashMap::new();
        let mut seen = HashSet::new();
        for slot in minds.values() {
            if seen.insert(Arc::as_ptr(&slot.record.settings)) {
                let settings = &slot.record.settings;
                shared.insert(settings.digest(), Arc::downgrade(settings));
            }
        }
        tracing::info!(minds = minds.len(), "the host is open");
        let inner = Inner {
            store,
            model,
            config,
            minds: Mutex::new(minds),
            links: AtomicU64::new(1),
            waits: std::sync::Mutex::default(),
            shared: std::sync::Mutex::new(shared),
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
            let store = Arc::clone(&host.inner.store);
            let working = blocking(move || {
                // A mind that has ended has nothing to do.
                let live = store.live()?;
                let working = live.into_iter().filter(|name| match store.recent(name) {
                    Ok(latest) => {
                        let status = Mind::status(name.clone(), latest);
                        matches!(status.state, State::Thinking | State::Working)
                    }
                    Err(error) => {
                        tracing::warn!(mind = %name, %error, "a mind's history could not be read");
                        false
                    }
                });
                Ok(working.collect::<Vec<_>>())
            })
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "the minds that have not ended could not be read");
                Vec::new()
            });
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

    /// Every mind that has not ended, in the order of their names.
    pub async fn minds(&self) -> Result<Vec<MindStatus>, HostError> {
        let store = Arc::clone(&self.inner.store);
        let live = blocking(move || store.live())
            .await
            .map_err(HostError::Store)?;
        let live: BTreeSet<Name> = live.into_iter().collect();
        let known: Vec<Known> = {
            let minds = self.inner.minds.lock().await;
            let live = live.iter().filter_map(|name| minds.get(name));
            live.map(Known::of).collect()
        };
        let mut statuses = Vec::with_capacity(known.len());
        for known in known {
            if let Ok(status) = self.status(known).await {
                statuses.push(status);
            }
        }
        Ok(statuses)
    }

    /// One mind.
    pub async fn mind(&self, name: &Name) -> Result<MindStatus, HostError> {
        self.status(self.known(name).await?).await
    }

    /// What the host knows of the mind named `name`.
    async fn known(&self, name: &Name) -> Result<Known, HostError> {
        let minds = self.inner.minds.lock().await;
        let slot = minds.get(name);
        Ok(Known::of(
            slot.ok_or_else(|| HostError::Missing(name.clone()))?,
        ))
    }

    /// What a mind is doing, as the API says it.
    async fn status(&self, known: Known) -> Result<MindStatus, HostError> {
        let record = known.record.clone();
        let task::Status { status, hands } = self.read(known).await?;
        Ok(MindStatus {
            mind: record,
            state: status.state,
            hands,
            waiting: status.waiting,
            watching: status.watching,
            length: status.length,
        })
    }

    /// What a mind is doing, and the hands connected. A mind that is awake
    /// says. One that rests is read from the store, and stays at rest: it
    /// has no hand connected.
    async fn read(&self, known: Known) -> Result<task::Status, HostError> {
        let Known { record, awake } = known;
        let name = record.name;
        if let Some(commands) = awake {
            let stopped = || HostError::Stopped(name.clone());
            let (reply, answer) = oneshot::channel();
            commands
                .send(Command::Status { reply })
                .map_err(|_| stopped())?;
            return answer.await.map_err(|_| stopped());
        }
        // The latest of its history says what a mind is doing: far less
        // than it would be loaded with.
        let store = Arc::clone(&self.inner.store);
        let read = blocking(move || {
            Ok(task::Status {
                status: Mind::status(name.clone(), store.recent(&name)?),
                hands: Vec::new(),
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

    /// Leaves older results out of a mind's requests from now on: each but
    /// the newest `keep`, or as many as its own trim keeps, or none. The
    /// trim is stored when this returns, if the mind had a result to leave
    /// out.
    pub async fn trim(&self, name: &Name, keep: Option<u64>) -> Result<MindStatus, HostError> {
        let commands = self.reach(name).await?;
        let (reply, answer) = oneshot::channel();
        let stopped = || HostError::Stopped(name.clone());
        commands
            .send(Command::Trim { keep, reply })
            .map_err(|_| stopped())?;
        drop(commands);
        answer.await.map_err(|_| stopped())?;
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

    /// Each mind's `wait`, by the name of the mind that waits.
    fn waits(&self) -> std::sync::MutexGuard<'_, BTreeMap<Name, Wait>> {
        let waits = self.inner.waits.lock();
        waits.unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Whether a mind waits for `mind` to rest.
    pub(crate) fn is_watched(&self, mind: &Name) -> bool {
        let waits = self.waits();
        waits.values().any(|wait| wait.unheard.contains(mind))
    }

    /// `parent` waits under `key` for each of `minds` to rest: each tells
    /// it what it said once it is, at once if it already is. Once `timeout`
    /// has passed, the parent is told what each is doing, and woken for it
    /// if it rests. A parent that rested and woke watches again under the
    /// same key, and its wait keeps the timer it had.
    pub(crate) fn watch(&self, parent: &Name, key: String, minds: &[Name], timeout: Duration) {
        {
            let mut waits = self.waits();
            let unheard = minds.iter().cloned().collect();
            if let Some(wait) = waits.get_mut(parent)
                && wait.key == key
            {
                wait.unheard = unheard;
            } else {
                let timer = self.time_out(parent.clone(), key.clone(), minds.to_vec(), timeout);
                let wait = Wait {
                    key,
                    unheard,
                    timer,
                };
                if let Some(ended) = waits.insert(parent.clone(), wait) {
                    ended.timer.abort();
                }
            }
        }
        for mind in minds {
            let (host, mind) = (self.clone(), mind.clone());
            tokio::spawn(async move {
                // One that rests from now on says so itself.
                if let Some(report) = host.report_now(&mind).await
                    && report.rests()
                {
                    host.rested(&mind, &report);
                }
            });
        }
    }

    /// `parent`'s wait under `key` has ended: the minds it waited for say
    /// nothing more to it, and its timer stops.
    pub(crate) fn unwatch(&self, parent: &Name, key: &str) {
        let mut waits = self.waits();
        if let Entry::Occupied(wait) = waits.entry(parent.clone())
            && wait.get().key == key
        {
            wait.remove().timer.abort();
        }
    }

    /// Starts the timer of `parent`'s wait under `key`: once `timeout` has
    /// passed, the parent is told what each of `minds` is doing, and woken
    /// for it if it rests. A wait that ends first stops it.
    fn time_out(
        &self,
        parent: Name,
        key: String,
        minds: Vec<Name>,
        timeout: Duration,
    ) -> AbortHandle {
        let host = self.clone();
        let timer = tokio::spawn(async move {
            let mut stopped = host.inner.stopped.subscribe();
            tokio::select! {
                () = tokio::time::sleep(timeout) => {}
                () = task::stopped(&mut stopped) => return,
            }
            let mut now = BTreeMap::new();
            for mind in minds {
                if let Some(report) = host.report_now(&mind).await {
                    now.insert(mind, report);
                }
            }
            match host.reach(&parent).await {
                Ok(commands) => {
                    let _ = commands.send(Command::TimedOut { key, now });
                }
                Err(error) => {
                    tracing::warn!(%parent, %error, "a mind that waits could not be told");
                }
            }
        });
        timer.abort_handle()
    }

    /// `mind` is at rest, having said `report`: every wait that has not
    /// heard from it is told.
    pub(crate) fn rested(&self, mind: &Name, report: &Report) {
        let mut told = Vec::new();
        for (parent, wait) in self.waits().iter_mut() {
            if wait.unheard.remove(mind) {
                told.push((parent.clone(), wait.key.clone()));
            }
        }
        for (parent, key) in told {
            let (host, mind, report) = (self.clone(), mind.clone(), report.clone());
            tokio::spawn(async move {
                match host.reach(&parent).await {
                    Ok(commands) => {
                        let _ = commands.send(Command::Rested { key, mind, report });
                    }
                    Err(error) => {
                        tracing::warn!(%parent, %error, "a mind that waits could not be told");
                    }
                }
            });
        }
    }

    /// What a mind that waits for `mind` is told of it now, as
    /// [`mensor_mind::Status::report_now`] says it; nothing when it cannot
    /// be read.
    async fn report_now(&self, mind: &Name) -> Option<Report> {
        let read = async { self.read(self.known(mind).await?).await };
        match read.await {
            Ok(task::Status { status, .. }) => Some(status.report_now()),
            Err(HostError::Missing(_)) => Some(Report::Missing),
            Err(error) => {
                tracing::warn!(%mind, %error, "a mind waited for could not be told of");
                None
            }
        }
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
            .get_or_insert_with(|| task::spawn(&slot.record, &self.inner));
        awake.clone()
    }

    /// Makes a mind named `name`, a root, with `settings` of its own, and
    /// from the first items of another's history if `from` says so. A mind
    /// made so before is the one made, if it was made the same way: else
    /// it is refused. Its settings are fixed from then on.
    ///
    /// A mind made from another shares its first `at` items, and keeps its
    /// hand `hand`, which says hello as it again: see [`Mind::branch`].
    /// Each setting it is not given is that mind's.
    pub async fn make(
        &self,
        name: &Name,
        settings: MindSettings,
        from: Option<MadeFrom>,
    ) -> Result<Made, HostError> {
        if !name.is_root() {
            return Err(HostError::NotRoot(name.clone()));
        }
        let mut minds = self.inner.minds.lock().await;
        let record = match &from {
            None => MindRecord {
                name: name.clone(),
                origin: Origin::Root,
                settings: self.share(settings),
            },
            Some(from) => {
                let keeper = minds.get(&from.mind);
                let keeper = keeper.ok_or_else(|| HostError::Missing(from.mind.clone()))?;
                // An effort is in its model's words, so it goes with the
                // model: one that names another model takes none from the
                // keeper.
                let mut kept = MindSettings::clone(&keeper.record.settings);
                let keepers = kept.model.as_deref().unwrap_or(self.inner.model.name());
                if settings
                    .model
                    .as_deref()
                    .is_some_and(|model| model != keepers)
                {
                    kept.effort = None;
                }
                MindRecord {
                    name: name.clone(),
                    origin: Origin::From { from: from.clone() },
                    settings: self.share(settings.or(&kept)),
                }
            }
        };
        if let Some(made) = minds.get(name) {
            return if made.record == record {
                Ok(Made::Existing)
            } else {
                Err(HostError::Exists(name.clone()))
            };
        }
        self.offers(&record.settings)?;
        let items = match from {
            None => Vec::new(),
            Some(from) => {
                // Its calls are numbered above every one its keeper made, so
                // a hand restored from later than `at` reads them as new.
                let (store, of, at) = (Arc::clone(&self.inner.store), from.mind.clone(), from.at);
                let (tail, last) = blocking(move || {
                    let last = match store.last(&of, "call", u64::MAX)? {
                        Some(Item::Call { id, .. }) => id,
                        _ => 0,
                    };
                    Ok((store.tail_at(&of, at)?, last))
                })
                .await
                .map_err(HostError::Store)?;
                Mind::branch(&from, tail, last, &self.inner.config.mind)?
            }
        };
        let (store, made) = (Arc::clone(&self.inner.store), record.clone());
        blocking(move || store.create(&made, &items))
            .await
            .map_err(|error| match error {
                StoreError::Exists(name) => HostError::Exists(name),
                StoreError::Missing(name) => HostError::Missing(name),
                other => HostError::Store(other),
            })?;
        tracing::info!(mind = %name, origin = ?record.origin, "a mind was made");
        minds.insert(
            name.clone(),
            Slot {
                record,
                awake: None,
            },
        );
        Ok(Made::Made)
    }

    /// `settings`, as shared by every mind made with them.
    fn share(&self, settings: MindSettings) -> Arc<MindSettings> {
        let shared = self.inner.shared.lock();
        let mut shared = shared.unwrap_or_else(std::sync::PoisonError::into_inner);
        let digest = settings.digest();
        if let Some(settings) = shared.get(&digest).and_then(Weak::upgrade) {
            return settings;
        }
        let settings = Arc::new(settings);
        shared.insert(digest, Arc::downgrade(&settings));
        settings
    }

    /// Refuses settings the host cannot ask with: a model it does not
    /// offer, or an effort the API of the model asked does not take.
    fn offers(&self, settings: &MindSettings) -> Result<(), HostError> {
        let (own, models) = (&self.inner.model, &self.inner.config.models);
        if let Some(asked) = settings.model.as_deref()
            && asked != own.name()
            && !models.contains_key(asked)
        {
            return Err(HostError::Model {
                asked: asked.to_owned(),
                offered: std::iter::once(own.name().to_owned())
                    .chain(models.keys().cloned())
                    .collect(),
            });
        }
        let model = self.inner.asking(settings.model.as_deref());
        if let Some(effort) = &settings.effort {
            let taken = model.efforts();
            let known = taken.is_none_or(|taken| taken.contains(&effort.as_str()));
            if effort.is_empty() || !known {
                return Err(HostError::Effort {
                    asked: effort.clone(),
                    taken: taken.map(|taken| taken.join(", ")),
                });
            }
        }
        Ok(())
    }

    /// Ends a mind: its turn stops, every hand is let go, and from then on
    /// it asks its model nothing and takes no prompt. Its history stays,
    /// and so do the minds that share it. Ending one that has ended does
    /// nothing.
    pub async fn end(&self, name: &Name) -> Result<MindStatus, HostError> {
        let commands = self.reach(name).await?;
        let (reply, answer) = oneshot::channel();
        let stopped = || HostError::Stopped(name.clone());
        commands
            .send(Command::End { reply })
            .map_err(|_| stopped())?;
        drop(commands);
        answer.await.map_err(|_| stopped())?;
        self.mind(name).await
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
                let record = MindRecord::root(mind.clone());
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

    /// A model behind Claude's Messages API, which is never asked here.
    #[derive(Debug)]
    struct Claude;

    impl Model for Claude {
        fn call(&self, _: mensor_mind::ModelRequest, _: crate::Wrote) -> crate::ModelFuture<'_> {
            Box::pin(async { Err(crate::ModelError("never asked".into())) })
        }

        fn name(&self) -> &'static str {
            "claude-opus-5-5"
        }

        fn efforts(&self) -> Option<&'static [&'static str]> {
            mensor_model::Api::Messages.efforts()
        }
    }

    fn name(name: &str) -> Name {
        Name::new(name).unwrap()
    }

    fn with(model: Option<&str>, effort: Option<&str>) -> MindSettings {
        MindSettings {
            model: model.map(Into::into),
            effort: effort.map(Into::into),
            ..MindSettings::default()
        }
    }

    /// A host on Claude that offers Haiku, and GPT behind an API that takes
    /// any effort, over `store`.
    async fn offering(store: Arc<mensor_mind::MemoryStore>) -> Host {
        let config = Config {
            models: BTreeMap::from([
                (
                    "claude-haiku-5-5".to_owned(),
                    Arc::new(Claude) as Arc<dyn Model>,
                ),
                ("gpt-6.2-sol".to_owned(), Arc::new(crate::Scripted::new())),
            ]),
            ..Config::default()
        };
        Host::open(store, Arc::new(Claude), config).await.unwrap()
    }

    #[tokio::test]
    async fn a_mind_is_made_only_with_a_model_and_effort_its_host_offers() {
        let host = offering(Arc::default()).await;
        // Its own model, which it need not list, and ones it lists, each
        // with an effort its API takes.
        for (mind, model, effort) in [
            ("a", "claude-opus-5-5", "xhigh"),
            ("b", "claude-haiku-5-5", "low"),
            ("g", "gpt-6.2-sol", "minimal"),
        ] {
            let made = host
                .make(&name(mind), with(Some(model), Some(effort)), None)
                .await;
            assert!(made.is_ok(), "{made:?}");
        }
        let refused = host.make(&name("c"), with(Some("gpt-9"), None), None).await;
        assert!(
            matches!(refused, Err(HostError::Model { .. })),
            "{refused:?}"
        );
        for effort in ["minimal", "turbo", ""] {
            let asked = with(Some("claude-haiku-5-5"), Some(effort));
            let refused = host.make(&name("d"), asked, None).await;
            assert!(
                matches!(refused, Err(HostError::Effort { .. })),
                "{refused:?}"
            );
        }
        assert!(host.mind(&name("d")).await.is_err());
    }

    #[tokio::test]
    async fn a_mind_made_from_another_is_checked_on_the_settings_it_ends_up_with() {
        // Two minds a hand has joined: one on GPT, and one on a model the
        // host no longer offers.
        let store = Arc::new(mensor_mind::MemoryStore::new());
        for (mind, model, effort) in [
            ("gpt", "gpt-6.2-sol", Some("minimal")),
            ("old", "gone", None),
        ] {
            let record = MindRecord {
                name: name(mind),
                origin: Origin::Root,
                settings: Arc::new(with(Some(model), effort)),
            };
            let joined = Item::Attached {
                hand: name("vm"),
                boot: "b".into(),
                capabilities: Vec::new(),
            };
            store.create(&record, &[joined]).unwrap();
        }
        let host = offering(store).await;
        let from = |mind: &str| {
            Some(MadeFrom {
                mind: name(mind),
                at: 1,
                hand: name("vm"),
            })
        };
        let settings = async |mind: &str| {
            MindSettings::clone(&host.mind(&name(mind)).await.unwrap().mind.settings)
        };

        // GPT's effort is checked as GPT's, on GPT.
        let made = host
            .make(&name("r1"), with(None, Some("minimal")), from("gpt"))
            .await;
        assert!(matches!(made, Ok(Made::Made)), "{made:?}");
        let again = host
            .make(&name("r1"), with(None, Some("minimal")), from("gpt"))
            .await;
        assert!(matches!(again, Ok(Made::Existing)), "{again:?}");
        // An effort goes with its model.
        let same = host
            .make(&name("r2"), with(Some("gpt-6.2-sol"), None), from("gpt"))
            .await;
        assert!(matches!(same, Ok(Made::Made)), "{same:?}");
        assert_eq!(
            settings("r2").await,
            with(Some("gpt-6.2-sol"), Some("minimal"))
        );
        let switched = host
            .make(
                &name("r3"),
                with(Some("claude-haiku-5-5"), None),
                from("gpt"),
            )
            .await;
        assert!(matches!(switched, Ok(Made::Made)), "{switched:?}");
        assert_eq!(settings("r3").await, with(Some("claude-haiku-5-5"), None));
        // A model no longer offered is not made into another mind, but a
        // mind made before is made, the same way.
        let refused = host.make(&name("r4"), with(None, None), from("old")).await;
        assert!(
            matches!(refused, Err(HostError::Model { .. })),
            "{refused:?}"
        );
        let again = host
            .make(&name("old"), with(Some("gone"), None), None)
            .await;
        assert!(matches!(again, Ok(Made::Existing)), "{again:?}");
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

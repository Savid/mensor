use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use mensor_api::{MindStatus, PAGE, Prompted};
use mensor_mind::{Conflict, Item, Mind, MindRecord, NoHand, Origin, Settings, Store, StoreError};
use mensor_protocol::{FromHand, Hello, InvalidName, Name, Refusal};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};

use crate::model::Model;
use crate::task::{self, Command, Link, Outgoing, Told, blocking};

/// How a host runs.
#[derive(Clone, Debug)]
pub struct Config {
    /// The bearer token every request must carry. Without one the host
    /// trusts whoever can reach it, so it must listen on loopback only.
    pub token: Option<String>,
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

pub(crate) struct Inner {
    pub(crate) store: Arc<dyn Store>,
    pub(crate) model: Arc<dyn Model>,
    pub(crate) config: Config,
    /// Every mind the store holds. A way to reach a mind's task is taken
    /// from here only while this is locked.
    pub(crate) minds: Mutex<BTreeMap<Name, Slot>>,
    links: AtomicU64,
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
    /// Opens a host over a store. Every mind the store holds rests until a
    /// hand dials in or something is asked of it.
    pub fn open(
        store: Arc<dyn Store>,
        model: Arc<dyn Model>,
        config: Config,
    ) -> Result<Self, StoreError> {
        let minds: BTreeMap<Name, Slot> = store
            .minds()?
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
        };
        Ok(Self {
            inner: Arc::new(inner),
        })
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
                length: status.length,
            });
        }
        // The latest of its history says what a mind is doing: far less
        // than it would be loaded with.
        let store = Arc::clone(&self.inner.store);
        let read = blocking(move || {
            let (from, items) = store.recent(&record.name)?;
            let status = Mind::status(record.name.clone(), from, items);
            Ok(MindStatus {
                mind: record,
                state: status.state,
                hands: Vec::new(),
                waiting: status.waiting,
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

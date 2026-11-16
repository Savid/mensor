use std::sync::{Arc, Weak};
use std::time::Duration;

use mensor_api::Writing;
use mensor_mind::{
    Conflict, Item, Mind, ModelReply, ModelRequest, Output, Pair, Settings, State, Store,
    StoreError,
};
use mensor_protocol::{FromHand, FromMind, Hello, Name, Refusal};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::AbortHandle;
use tokio::time::timeout;

use crate::host::Inner;
use crate::model::{Model, ModelError};

/// How much those following a pair may fall behind before they miss some
/// of what it tells. One that does reads the history again, and has lost
/// only text the next stored step holds.
const TOLD: usize = 256;

/// What a pair's task is asked to do.
pub(crate) enum Command {
    Prompt {
        key: Option<String>,
        text: String,
        reply: oneshot::Sender<Result<bool, Conflict>>,
    },
    Attach {
        hello: Box<Hello>,
        link: Link,
    },
    FromHand {
        link: u64,
        message: FromHand,
    },
    Detached {
        link: u64,
    },
    Model {
        seen: u64,
        result: Result<ModelReply, ModelError>,
    },
    Interrupt,
    Status {
        reply: oneshot::Sender<Status>,
    },
    Follow {
        reply: oneshot::Sender<broadcast::Receiver<Told>>,
    },
}

/// What a pair is doing.
pub(crate) struct Status {
    pub(crate) state: State,
    pub(crate) attached: bool,
    /// How many items of the pair's history are stored.
    pub(crate) length: u64,
}

/// What those following a pair are told, in the order it happened.
#[derive(Clone, Debug)]
pub(crate) enum Told {
    /// The history grew.
    Grew,
    /// The model wrote this of a reply that is not stored yet: a
    /// [`Writing`], as JSON.
    Wrote(Arc<str>),
}

/// One connection to a hand: its id, and the way to write to it.
pub(crate) struct Link {
    pub(crate) id: u64,
    pub(crate) outbound: mpsc::UnboundedSender<Outgoing>,
}

/// Something a mind's host has for a connected hand.
#[derive(Clone, Debug, PartialEq)]
pub enum Outgoing {
    /// A message for the hand.
    Message(FromMind),
    /// The host is ending the connection, for this reason.
    Close(Refusal),
}

/// What a pair's task starts with.
pub(crate) enum Start {
    /// The end of its history, read from the store.
    Stored,
    /// A mind just forked, the outputs that made it, and its hand's
    /// connection.
    Forked {
        mind: Box<Mind>,
        first: Vec<Output>,
        link: Link,
    },
}

/// The task of one pair that is awake. It holds the pair's mind, and is
/// the only one to change its history.
struct Task {
    name: Name,
    mind: Mind,
    store: Arc<dyn Store>,
    model: Arc<dyn Model>,
    /// The host, for the day the task leaves it.
    host: Weak<Inner>,
    /// How long the pair stays awake with nothing to stay awake for.
    rest_after: Duration,
    commands: mpsc::UnboundedReceiver<Command>,
    myself: mpsc::WeakUnboundedSender<Command>,
    link: Option<Link>,
    /// The request the model is answering now: what it had seen, and the
    /// way to give it up.
    asking: Option<(u64, AbortHandle)>,
    told: broadcast::Sender<Told>,
}

/// Runs one call to a store off the runtime's own threads, because a store
/// may wait on a disk.
pub(crate) async fn blocking<T: Send + 'static>(
    call: impl FnOnce() -> Result<T, StoreError> + Send + 'static,
) -> Result<T, StoreError> {
    let done = tokio::task::spawn_blocking(call).await;
    done.unwrap_or_else(|error| Err(StoreError::Backend(format!("a store call died: {error}"))))
}

/// Wakes one pair: starts its task, and gives the way to reach it.
///
/// The task serves commands until the pair has had nothing to stay awake
/// for (no hand, no follower, no model asked) for as long as the host lets
/// it rest, or until its history cannot be read or stored. Either way it
/// then leaves the host's memory, and the next thing that concerns the
/// pair wakes it from its store.
pub(crate) fn spawn(
    pair: &Pair,
    start: Start,
    host: &Arc<Inner>,
) -> mpsc::UnboundedSender<Command> {
    let (sender, commands) = mpsc::unbounded_channel();
    let task = Task {
        name: pair.name.clone(),
        mind: Mind::new(host.config.mind.clone()),
        store: Arc::clone(&host.store),
        model: Arc::clone(&host.model),
        host: Arc::downgrade(host),
        rest_after: host.config.rest_after,
        commands,
        myself: sender.downgrade(),
        link: None,
        asking: None,
        told: broadcast::Sender::new(TOLD),
    };
    tokio::spawn(task.run(start, host.config.mind.clone()));
    sender
}

impl Task {
    async fn run(mut self, start: Start, settings: Settings) {
        if let Err(error) = self.serve(start, settings).await {
            tracing::error!(pair = %self.name, %error, "the pair stopped: its history could not be read or stored");
            // Whoever asks next wakes the pair from what is stored.
            if let Some(host) = self.host.upgrade()
                && let Some(slot) = host.pairs.lock().await.get_mut(&self.name)
            {
                slot.awake = None;
            }
        }
    }

    async fn serve(&mut self, start: Start, settings: Settings) -> Result<(), StoreError> {
        match start {
            Start::Stored => {
                let (store, name) = (Arc::clone(&self.store), self.name.clone());
                let (from, items) = blocking(move || store.tail(&name)).await?;
                self.mind = Mind::load(from, items, settings);
            }
            Start::Forked { mind, first, link } => {
                self.mind = *mind;
                self.link = Some(link);
                self.perform(first).await?;
            }
        }
        loop {
            let resting =
                self.link.is_none() && self.asking.is_none() && self.told.receiver_count() == 0;
            let command = if resting {
                match timeout(self.rest_after, self.commands.recv()).await {
                    Ok(command) => command,
                    Err(_) if self.rest().await => return Ok(()),
                    Err(_) => continue,
                }
            } else {
                self.commands.recv().await
            };
            let Some(command) = command else {
                return Ok(());
            };
            self.handle(command).await?;
        }
    }

    /// Leaves the host's memory, unless someone is on their way here.
    /// `true` when the task is to end.
    ///
    /// A way to reach this task is handed out only under the lock taken
    /// here. So when the host holds the only one and nothing is queued,
    /// nobody can send to it after it has gone.
    async fn rest(&mut self) -> bool {
        let Some(host) = self.host.upgrade() else {
            return true;
        };
        let mut pairs = host.pairs.lock().await;
        if self.commands.sender_strong_count() > 1 || !self.commands.is_empty() {
            return false;
        }
        if let Some(slot) = pairs.get_mut(&self.name) {
            slot.awake = None;
        }
        tracing::debug!(pair = %self.name, "the pair rests");
        true
    }

    async fn handle(&mut self, command: Command) -> Result<(), StoreError> {
        match command {
            Command::Prompt { key, text, reply } => {
                // The mind knows the keys of the items it holds. One from
                // before them is in the store.
                let earlier = match &key {
                    Some(key) if self.mind.starts_at() > 0 => {
                        let (store, name, key) =
                            (Arc::clone(&self.store), self.name.clone(), key.clone());
                        blocking(move || store.prompt(&name, &key)).await?
                    }
                    _ => None,
                };
                match self.mind.prompt(key, text, earlier.as_deref()) {
                    Ok(prompted) => {
                        self.perform(prompted.outputs).await?;
                        let _ = reply.send(Ok(prompted.existing));
                    }
                    Err(conflict) => {
                        let _ = reply.send(Err(conflict));
                    }
                }
            }
            Command::Attach { hello, link } => {
                if let Some(old) = self.link.replace(link) {
                    let _ = old.outbound.send(Outgoing::Close(Refusal::Replaced));
                }
                tracing::info!(pair = %self.name, boot = %hello.boot, last_id = hello.last_id, "the hand attached");
                let outputs = self.mind.attach(&hello);
                self.perform(outputs).await?;
            }
            Command::FromHand { link, message } if self.is_current(link) => match message {
                FromHand::Result(result) => {
                    let outputs = self.mind.result(result);
                    self.perform(outputs).await?;
                }
                FromHand::Refused(refused) => {
                    tracing::warn!(pair = %self.name, call = refused.id, reason = %refused.reason, "the hand refused a call");
                    self.drop_link(Refusal::Protocol);
                }
                FromHand::Hello(_) => self.drop_link(Refusal::Protocol),
            },
            Command::Detached { link } if self.is_current(link) => {
                tracing::info!(pair = %self.name, "the hand detached");
                self.link = None;
                self.mind.detach();
            }
            Command::FromHand { .. } | Command::Detached { .. } => {}
            Command::Model { seen, result } => {
                self.asking.take_if(|(asked, _)| *asked == seen);
                let outputs = match result {
                    Ok(reply) => self.mind.replied(seen, reply),
                    Err(error) => {
                        tracing::warn!(pair = %self.name, %error, "the model could not be asked");
                        self.mind.failed(seen, error.to_string())
                    }
                };
                self.perform(outputs).await?;
            }
            Command::Interrupt => {
                let outputs = self.mind.interrupt();
                if !outputs.is_empty() {
                    tracing::info!(pair = %self.name, "the turn was interrupted");
                }
                // The request is given up, so its answer is not paid for
                // in full. One that lands anyway is one the mind drops.
                if let Some((_, asking)) = self.asking.take() {
                    asking.abort();
                }
                self.perform(outputs).await?;
            }
            Command::Status { reply } => {
                let status = Status {
                    state: self.mind.state(),
                    attached: self.mind.is_attached(),
                    length: self.mind.length(),
                };
                let _ = reply.send(status);
            }
            Command::Follow { reply } => {
                let _ = reply.send(self.told.subscribe());
            }
        }
        Ok(())
    }

    fn is_current(&self, link: u64) -> bool {
        self.link.as_ref().is_some_and(|current| current.id == link)
    }

    /// Ends the current connection. The hand settles again when it redials.
    fn drop_link(&mut self, refusal: Refusal) {
        if let Some(link) = self.link.take() {
            let _ = link.outbound.send(Outgoing::Close(refusal));
        }
        self.mind.detach();
    }

    /// Performs a mind's outputs in order. Items are stored before the
    /// output that follows them is performed.
    async fn perform(&mut self, outputs: Vec<Output>) -> Result<(), StoreError> {
        let mut items = Vec::new();
        for output in outputs {
            match output {
                Output::Append(item) => items.push(item),
                Output::Send(message) => {
                    self.store(&mut items).await?;
                    if let Some(link) = &self.link {
                        let _ = link.outbound.send(Outgoing::Message(message));
                    }
                }
                Output::Ask(request) => {
                    self.store(&mut items).await?;
                    self.ask(request);
                }
            }
        }
        self.store(&mut items).await
    }

    /// Appends `items` to the history, and returns once they are stored.
    async fn store(&mut self, items: &mut Vec<Item>) -> Result<(), StoreError> {
        if items.is_empty() {
            return Ok(());
        }
        let items = std::mem::take(items);
        let (store, name) = (Arc::clone(&self.store), self.name.clone());
        blocking(move || store.append(&name, &items)).await?;
        let _ = self.told.send(Told::Grew);
        Ok(())
    }

    /// Asks the model on a task of its own. Those following the pair are
    /// told what it writes, unless it writes a summary, which is no reply.
    fn ask(&mut self, request: ModelRequest) {
        let model = Arc::clone(&self.model);
        let myself = self.myself.clone();
        let (told, quiet) = (self.told.clone(), request.text_only);
        let wrote = move |at: u64, text: &str| {
            if quiet || told.receiver_count() == 0 {
                return;
            }
            if let Ok(writing) = serde_json::to_string(&Writing { at, text }) {
                let _ = told.send(Told::Wrote(writing.into()));
            }
        };
        let seen = request.seen;
        let asking = tokio::spawn(async move {
            let result = model.call(request, Box::new(wrote)).await;
            if let Some(commands) = myself.upgrade() {
                let _ = commands.send(Command::Model { seen, result });
            }
        });
        self.asking = Some((seen, asking.abort_handle()));
    }
}

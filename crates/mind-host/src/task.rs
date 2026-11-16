use std::collections::BTreeMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use mensor_api::Writing;
use mensor_mind::{
    Conflict, Fork, Item, Mind, MindRecord, ModelReply, ModelRequest, NoHand, Origin, Output,
    Report, State, Store, StoreError,
};
use mensor_protocol::{FromHand, FromMind, Hello, Name, Refusal};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::AbortHandle;
use tokio::time::timeout;

use crate::host::{Host, Inner, Slot};
use crate::model::{Model, ModelError};

/// How much those following a mind may fall behind before they miss some
/// of what it tells. One that does reads the history again, and has lost
/// only text the next stored step holds.
const TOLD: usize = 256;

/// What a mind's task is asked to do.
pub(crate) enum Command {
    Prompt {
        key: Option<String>,
        text: String,
        /// The mind that says it, when its parent or a child does.
        from: Option<Name>,
        reply: oneshot::Sender<Result<bool, Conflict>>,
    },
    Attach {
        hello: Box<Hello>,
        link: Link,
    },
    FromHand {
        hand: Name,
        link: u64,
        message: FromHand,
    },
    Detached {
        hand: Name,
        link: u64,
    },
    Model {
        seen: u64,
        result: Result<ModelReply, ModelError>,
    },
    /// A message the mind said reached the mind it was for, or could not.
    Told {
        key: String,
        result: Result<(), String>,
    },
    /// A mind this one watches is at rest.
    Rested {
        mind: Name,
        report: Report,
    },
    Interrupt,
    Release {
        hand: Name,
        reply: oneshot::Sender<Result<(), NoHand>>,
    },
    Status {
        reply: oneshot::Sender<Status>,
    },
    Follow {
        reply: oneshot::Sender<broadcast::Receiver<Told>>,
    },
}

/// What a mind is doing.
pub(crate) struct Status {
    pub(crate) state: State,
    /// The hands connected.
    pub(crate) hands: Vec<Name>,
    /// The hands its turn waits for.
    pub(crate) waiting: Vec<Name>,
    /// The children its turn waits for.
    pub(crate) watching: Vec<Name>,
    /// How many items of its history are stored.
    pub(crate) length: u64,
    /// What it said when it came to rest, while it is at rest.
    pub(crate) report: Option<Report>,
}

/// What those following a mind are told, in the order it happened.
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

/// The task of one mind that is awake. It holds the mind, and is the only
/// one to change its history.
struct Task {
    name: Name,
    mind: Mind,
    store: Arc<dyn Store>,
    model: Arc<dyn Model>,
    /// The host, for the day the task leaves it.
    host: Weak<Inner>,
    /// How long the mind stays awake with nothing to stay awake for.
    rest_after: Duration,
    commands: mpsc::UnboundedReceiver<Command>,
    myself: mpsc::WeakUnboundedSender<Command>,
    /// The connection of each hand that is connected.
    links: BTreeMap<Name, Link>,
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

/// Wakes one mind: starts its task, which loads the end of its history, and
/// gives the way to reach it.
///
/// The task serves commands until the mind has had nothing to stay awake
/// for (no hand, no follower, no model asked) for as long as the host lets
/// it rest, or until its history cannot be read or stored. Either way it
/// then leaves the host's memory, and the next thing that concerns the
/// mind wakes it from its store.
pub(crate) fn spawn(name: &Name, host: &Arc<Inner>) -> mpsc::UnboundedSender<Command> {
    let (sender, commands) = mpsc::unbounded_channel();
    let task = Task {
        name: name.clone(),
        mind: Mind::new(name.clone(), host.config.mind.clone()),
        store: Arc::clone(&host.store),
        model: Arc::clone(&host.model),
        host: Arc::downgrade(host),
        rest_after: host.config.rest_after,
        commands,
        myself: sender.downgrade(),
        links: BTreeMap::new(),
        asking: None,
        told: broadcast::Sender::new(TOLD),
    };
    tokio::spawn(task.run());
    sender
}

impl Task {
    async fn run(mut self) {
        if let Err(error) = self.serve().await {
            tracing::error!(mind = %self.name, %error, "the mind stopped: its history could not be read or stored");
            // Whoever asks next wakes the mind from what is stored.
            if let Some(host) = self.host.upgrade()
                && let Some(slot) = host.minds.lock().await.get_mut(&self.name)
            {
                slot.awake = None;
            }
        }
    }

    async fn serve(&mut self) -> Result<(), StoreError> {
        let (store, name) = (Arc::clone(&self.store), self.name.clone());
        let (from, items) = blocking(move || store.tail(&name)).await?;
        let settings = self.host.upgrade().map(|host| host.config.mind.clone());
        self.mind = Mind::load(self.name.clone(), from, items, settings.unwrap_or_default());
        let outputs = self.mind.start();
        self.perform(outputs).await?;
        loop {
            let resting =
                self.links.is_empty() && self.asking.is_none() && self.told.receiver_count() == 0;
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
        let mut minds = host.minds.lock().await;
        if self.commands.sender_strong_count() > 1 || !self.commands.is_empty() {
            return false;
        }
        if let Some(slot) = minds.get_mut(&self.name) {
            slot.awake = None;
        }
        tracing::debug!(mind = %self.name, "the mind rests");
        true
    }

    async fn handle(&mut self, command: Command) -> Result<(), StoreError> {
        match command {
            Command::Prompt {
                key,
                text,
                from,
                reply,
            } => {
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
                match self.mind.prompt(key, text, from, earlier.as_deref()) {
                    Ok(prompted) => {
                        self.perform(prompted.outputs).await?;
                        let _ = reply.send(Ok(prompted.existing));
                    }
                    Err(conflict) => {
                        let _ = reply.send(Err(conflict));
                    }
                }
            }
            Command::Attach { hello, link } => self.attach(&hello, link).await?,
            Command::FromHand {
                hand,
                link,
                message,
            } if self.is_current(&hand, link) => self.heard(hand, message).await?,
            Command::Detached { hand, link } if self.is_current(&hand, link) => {
                tracing::info!(mind = %self.name, %hand, "a hand detached");
                self.links.remove(&hand);
                self.mind.detach(&hand);
            }
            Command::FromHand { .. } | Command::Detached { .. } => {}
            Command::Model { seen, result } => {
                self.asking.take_if(|(asked, _)| *asked == seen);
                let outputs = match result {
                    Ok(reply) => self.mind.replied(seen, reply),
                    Err(error) => {
                        tracing::warn!(mind = %self.name, %error, "the model could not be asked");
                        self.mind.failed(seen, error.to_string())
                    }
                };
                self.perform(outputs).await?;
            }
            Command::Told { key, result } => {
                let outputs = self.mind.told(&key, result);
                self.perform(outputs).await?;
            }
            Command::Rested { mind, report } => {
                let outputs = self.mind.rested(&mind, report);
                self.perform(outputs).await?;
            }
            Command::Interrupt => {
                let outputs = self.mind.interrupt();
                if !outputs.is_empty() {
                    tracing::info!(mind = %self.name, "the turn was interrupted");
                }
                // The request is given up, so its answer is not paid for
                // in full. One that lands anyway is one the mind drops.
                if let Some((_, asking)) = self.asking.take() {
                    asking.abort();
                }
                self.perform(outputs).await?;
            }
            Command::Release { hand, reply } => match self.mind.release(&hand) {
                Ok(outputs) => {
                    tracing::info!(mind = %self.name, %hand, "a hand was released");
                    self.perform(outputs).await?;
                    let _ = reply.send(Ok(()));
                }
                Err(error) => {
                    let _ = reply.send(Err(error));
                }
            },
            Command::Status { reply } => {
                let status = self.mind.status_now();
                let _ = reply.send(Status {
                    state: status.state,
                    hands: self.mind.connected(),
                    waiting: status.waiting,
                    watching: status.watching,
                    length: status.length,
                    report: status.report,
                });
            }
            Command::Follow { reply } => {
                let _ = reply.send(self.told.subscribe());
            }
        }
        Ok(())
    }

    /// Joins a hand that said hello, or turns it away.
    async fn attach(&mut self, hello: &Hello, link: Link) -> Result<(), StoreError> {
        let hand = hello.hand.clone();
        match self.mind.attach(hello) {
            Ok(outputs) => {
                if let Some(old) = self.links.insert(hand.clone(), link) {
                    let _ = old.outbound.send(Outgoing::Close(Refusal::Replaced));
                }
                tracing::info!(mind = %self.name, %hand, boot = %hello.boot, last_id = hello.last_id, "a hand attached");
                self.perform(outputs).await
            }
            Err(refusal) => {
                tracing::info!(mind = %self.name, %hand, reason = refusal.reason(), "a hand was refused");
                let _ = link.outbound.send(Outgoing::Close(refusal));
                Ok(())
            }
        }
    }

    /// Takes a message from a connected hand.
    async fn heard(&mut self, hand: Name, message: FromHand) -> Result<(), StoreError> {
        match message {
            FromHand::Result(result) => {
                let outputs = self.mind.result(&hand, result);
                self.perform(outputs).await?;
            }
            FromHand::Refused(refused) => {
                tracing::warn!(mind = %self.name, %hand, call = refused.id, reason = %refused.reason, "a hand refused a call");
                self.drop_link(&hand, Refusal::Protocol);
            }
            FromHand::Hello(_) => self.drop_link(&hand, Refusal::Protocol),
        }
        Ok(())
    }

    fn is_current(&self, hand: &Name, link: u64) -> bool {
        self.links
            .get(hand)
            .is_some_and(|current| current.id == link)
    }

    /// Ends a hand's connection. The hand settles again when it redials.
    fn drop_link(&mut self, hand: &Name, refusal: Refusal) {
        if let Some(link) = self.links.remove(hand) {
            let _ = link.outbound.send(Outgoing::Close(refusal));
        }
        self.mind.detach(hand);
    }

    /// Performs a mind's outputs in order. Items are stored, and children
    /// made, before the output that follows them is performed. A mind that
    /// is then at rest tells those that wait for it.
    async fn perform(&mut self, outputs: Vec<Output>) -> Result<(), StoreError> {
        self.carry_out(outputs).await?;
        if let Some(host) = self.host.upgrade().map(Host::from)
            && host.is_watched(&self.name)
            && let Some(report) = self.mind.report()
        {
            host.rested(&self.name, &report);
        }
        Ok(())
    }

    async fn carry_out(&mut self, outputs: Vec<Output>) -> Result<(), StoreError> {
        let mut items = Vec::new();
        for output in outputs {
            match output {
                Output::Append(item) => items.push(item),
                Output::Send { hand, message } => {
                    self.store(&mut items).await?;
                    if let Some(link) = self.links.get(&hand) {
                        let _ = link.outbound.send(Outgoing::Message(message));
                    }
                }
                Output::Close { hand, refusal } => {
                    self.store(&mut items).await?;
                    if let Some(link) = self.links.remove(&hand) {
                        let _ = link.outbound.send(Outgoing::Close(refusal));
                    }
                }
                Output::Fork(fork) => {
                    self.store(&mut items).await?;
                    self.fork(fork).await?;
                }
                Output::Ask(request) => {
                    self.store(&mut items).await?;
                    self.ask(request);
                }
                Output::Tell { to, key, text } => {
                    self.store(&mut items).await?;
                    self.tell(to, key, text);
                }
                Output::Watch(minds) => {
                    self.store(&mut items).await?;
                    if let Some(host) = self.host.upgrade() {
                        Host::from(host).watch(&self.name, minds);
                    }
                }
            }
        }
        self.store(&mut items).await
    }

    /// Makes a child mind, in one write with its first items, and adds it
    /// to the host's minds, resting. A child the store already has with
    /// this origin was made before a stop, and is the one made.
    async fn fork(&mut self, fork: Fork) -> Result<(), StoreError> {
        let Fork {
            name,
            parent,
            inherited,
            items,
        } = fork;
        let record = MindRecord {
            name: name.clone(),
            origin: Origin::Fork { parent, inherited },
        };
        let (store, made) = (Arc::clone(&self.store), record.clone());
        blocking(move || store.create(&made, &items)).await?;
        tracing::info!(mind = %self.name, child = %name, inherited, "a child mind was made");
        if let Some(host) = self.host.upgrade() {
            let mut minds = host.minds.lock().await;
            minds.entry(name).or_insert(Slot {
                record,
                awake: None,
            });
        }
        Ok(())
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

    /// Says a message to another mind on a task of its own: the two may be
    /// telling each other at once.
    fn tell(&self, to: Name, key: String, text: String) {
        let Some(host) = self.host.upgrade() else {
            return;
        };
        let (host, from, myself) = (Host::from(host), self.name.clone(), self.myself.clone());
        tokio::spawn(async move {
            let result = host.say(&to, Some(key.clone()), text, Some(from)).await;
            let result = result.map(|_| ()).map_err(|error| error.to_string());
            if let Some(commands) = myself.upgrade() {
                let _ = commands.send(Command::Told { key, result });
            }
        });
    }

    /// Asks the model on a task of its own. Those following the mind are
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

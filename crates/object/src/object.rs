use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;

use mensor_hand::{Capabilities, Core, Effect, Limits};
use mensor_mind::{
    Conflict, Item, Mind, ModelError, ModelReply, ModelRequest, Output, Settings, State,
};
use mensor_protocol::{Call, CallId, CapabilityDecl, FromHand, FromMind, Name, Reply};

use crate::{Storage, StorageError, Stored};

/// The name of an object's one hand.
pub const LOCAL: &str = "local";

fn local() -> Name {
    Name::new(LOCAL).expect("local is a name")
}

/// A language model an object asks.
///
/// A model retries what is worth retrying itself. An error it returns ends
/// the mind's turn until the next prompt.
pub trait Model {
    /// Asks the model once.
    fn ask(&self, request: &ModelRequest) -> impl Future<Output = Result<ModelReply, ModelError>>;
}

/// One piece of work an object has for its host.
#[derive(Clone, Debug, PartialEq)]
pub enum Work {
    /// Ask the model, and answer with [`Object::replied`].
    Ask(ModelRequest),
    /// Run this call on the hand's capabilities, and answer with
    /// [`Object::finished`].
    Call(Call),
}

/// Why a prompt was not taken.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PromptError {
    /// Another prompt already has this key, with other text.
    #[error(transparent)]
    Conflict(#[from] Conflict),
    /// The prompt could not be stored.
    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// A mind and its one hand in one place: the mind, what its hand remembers,
/// and the storage that holds both. The hand is named [`LOCAL`], and the
/// mind has the object's name.
#[derive(Debug)]
pub struct Object<S> {
    mind: Mind,
    hand: Core,
    declared: Vec<CapabilityDecl>,
    storage: S,
    work: VecDeque<Work>,
}

impl<S: Storage> Object<S> {
    /// Opens the object stored in `storage`. `None` when nothing is stored
    /// there: no mind has started.
    ///
    /// An object that is opened again is the same hand, with everything it
    /// remembered. Calls it was running when it stopped are lost, and the
    /// mind is told so. `declared` is what the hand can do now, and
    /// `settings` what the mind is told.
    pub fn open(
        declared: Vec<CapabilityDecl>,
        settings: Settings,
        mut storage: S,
    ) -> Result<Option<Self>, StorageError> {
        let Stored { from, items, hand } = storage.load()?;
        let Some(mut hand) = hand else {
            return Ok(None);
        };
        if !hand.interrupted().is_empty() {
            storage.keep(&hand)?;
        }
        let mind = Mind::load(hand.mind().clone(), from, items, settings);
        Self::settled(mind, hand, declared, storage).map(Some)
    }

    /// Opens the object stored in `storage`, or starts one named `name`
    /// there. `boot` is asked for a random string only for a new one.
    pub fn start(
        name: Name,
        boot: impl FnOnce() -> String,
        declared: Vec<CapabilityDecl>,
        settings: Settings,
        mut storage: S,
    ) -> Result<Self, StorageError> {
        let Stored { from, items, hand } = storage.load()?;
        let mut hand = if let Some(hand) = hand {
            hand
        } else {
            let hand = Core::new(name, local(), boot(), Limits::default());
            storage.keep(&hand)?;
            hand
        };
        if !hand.interrupted().is_empty() {
            storage.keep(&hand)?;
        }
        let mind = Mind::load(hand.mind().clone(), from, items, settings);
        Self::settled(mind, hand, declared, storage)
    }

    fn settled(
        mind: Mind,
        hand: Core,
        declared: Vec<CapabilityDecl>,
        storage: S,
    ) -> Result<Self, StorageError> {
        let mut object = Self {
            mind,
            hand,
            declared,
            storage,
            work: VecDeque::new(),
        };
        object.settle()?;
        Ok(object)
    }

    /// The mind's name.
    pub fn name(&self) -> &Name {
        self.hand.mind()
    }

    /// How many items the history holds.
    pub fn length(&self) -> u64 {
        self.mind.length()
    }

    /// The history from index `from`, `limit` items of it at the most. It
    /// is read from the storage: the object holds only its end.
    pub fn read(&self, from: u64, limit: usize) -> Result<Vec<Item>, StorageError> {
        self.storage.read(from, limit)
    }

    /// What the mind is doing.
    pub fn state(&self) -> State {
        self.mind.state()
    }

    /// Whether [`Object::take`] has work to give.
    pub fn has_work(&self) -> bool {
        !self.work.is_empty()
    }

    /// Says something to the mind. `true` when the history already held this
    /// prompt under its key, so nothing was added.
    pub fn prompt(&mut self, key: Option<String>, text: String) -> Result<bool, PromptError> {
        // The mind knows the keys of the items it holds. One from before
        // them is in the storage.
        let earlier = match &key {
            Some(key) if self.mind.starts_at() > 0 => self.storage.prompt(key)?,
            _ => None,
        };
        let prompted = self.mind.prompt(key, text, earlier.as_deref())?;
        self.perform(prompted.outputs)?;
        Ok(prompted.existing)
    }

    /// Stops the turn, as [`Mind::interrupt`] does. Work the host has not
    /// taken is dropped. Work it is doing ends unheard: what it reports
    /// afterwards changes nothing, so a host may stop waiting for it.
    pub fn interrupt(&mut self) -> Result<(), StorageError> {
        let outputs = self.mind.interrupt();
        self.work.retain(|work| !matches!(work, Work::Ask(_)));
        self.perform(outputs)
    }

    /// Takes the next piece of work. The host does it and reports how it
    /// ended. Work a host takes and never reports is settled when the object
    /// is next opened.
    pub fn take(&mut self) -> Option<Work> {
        self.work.pop_front()
    }

    /// Reports the model's answer to the request that carried `seen`.
    pub fn replied(
        &mut self,
        seen: u64,
        answer: Result<ModelReply, ModelError>,
    ) -> Result<(), StorageError> {
        let outputs = match answer {
            Ok(reply) => self.mind.replied(seen, reply),
            Err(error) => self.mind.failed(seen, error.0),
        };
        self.perform(outputs)
    }

    /// Reports how a call ended.
    pub fn finished(&mut self, id: CallId, reply: Reply) -> Result<(), StorageError> {
        let Some(result) = self.hand.finished(id, reply) else {
            return Ok(());
        };
        let outputs = self.mind.result(&local(), result);
        self.perform(outputs)
    }

    /// The hand says hello, and the mind settles every call it waits on.
    /// The one hand is never released and is within any limit, so it is
    /// never refused.
    fn settle(&mut self) -> Result<(), StorageError> {
        let hello = self.hand.hello(self.declared.clone());
        let outputs = self.mind.attach(&hello).unwrap_or_default();
        self.perform(outputs)
    }

    /// Performs a mind's outputs in order. Items are stored before the
    /// output that follows them is performed.
    fn perform(&mut self, outputs: Vec<Output>) -> Result<(), StorageError> {
        let mut items = Vec::new();
        let mut refused = false;
        let mut cancelled = Vec::new();
        for output in outputs {
            match output {
                Output::Append(item) => items.push(item),
                Output::Send { .. } if refused => {}
                Output::Send { message, .. } => {
                    self.store(&mut items)?;
                    refused = self.deliver(message, &mut cancelled)?;
                }
                Output::Ask(request) => {
                    self.store(&mut items)?;
                    self.work.push_back(Work::Ask(request));
                }
                // The hand cannot clone, so its mind forks nothing and
                // lets no hand go.
                Output::Close { .. } | Output::Fork(_) => {}
            }
        }
        self.store(&mut items)?;
        for id in cancelled {
            // A call the host has not taken never starts. One it is running
            // is answered for here, and its own report is dropped.
            self.work
                .retain(|work| !matches!(work, Work::Call(call) if call.id == id));
            self.finished(id, Reply::Error("cancelled".into()))?;
        }
        if refused {
            // What a dropped connection does between a server and a remote
            // hand: the two say hello again, and the call the hand would not
            // start settles as one it may have run.
            self.mind.detach(&local());
            self.settle()?;
        }
        Ok(())
    }

    /// Hands the hand one message from the mind. `true` when the hand
    /// refused it. The calls it is told to stop join `cancelled`.
    fn deliver(
        &mut self,
        message: FromMind,
        cancelled: &mut Vec<CallId>,
    ) -> Result<bool, StorageError> {
        let mut refused = false;
        for effect in self.hand.receive(message) {
            match effect {
                Effect::Start(call) => self.work.push_back(Work::Call(call)),
                Effect::Cancel(id) => cancelled.push(id),
                Effect::Send(FromHand::Refused(_)) => refused = true,
                // A hand's other messages are reported through `finished`
                // and `settle`.
                Effect::Send(_) => {}
            }
        }
        self.storage.keep(&self.hand)?;
        Ok(refused)
    }

    fn store(&mut self, items: &mut Vec<Item>) -> Result<(), StorageError> {
        if items.is_empty() {
            return Ok(());
        }
        self.storage.append(items)?;
        items.clear();
        Ok(())
    }
}

/// Does one piece of an object's work: asks the model once, or runs one
/// call. `false` when there was nothing to do.
///
/// The object is borrowed only between the awaits, so its host may prompt
/// it, or read it, while the model or a capability is working.
pub async fn step<S: Storage, M: Model>(
    object: &RefCell<Object<S>>,
    capabilities: &Capabilities,
    model: &M,
) -> Result<bool, StorageError> {
    let Some(work) = object.borrow_mut().take() else {
        return Ok(false);
    };
    match work {
        Work::Ask(request) => {
            let answer = model.ask(&request).await;
            object.borrow_mut().replied(request.seen, answer)?;
        }
        Work::Call(call) => {
            let reply = match capabilities.get(&call.capability) {
                Some(capability) => capability.call(&call.operation, call.input).await,
                None => Reply::Error(format!(
                    "this hand has no capability named {}",
                    call.capability
                )),
            };
            object.borrow_mut().finished(call.id, reply)?;
        }
    }
    Ok(true)
}

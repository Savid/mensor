use std::collections::{BTreeMap, BTreeSet};

use mensor_protocol::{
    Call, CallId, CallResult, FromMind, Hello, Name, Refusal, Reply, Settled, settle,
};
use serde_json::{Value, json};

use crate::fork::{Fork, child_name, shares};
use crate::tools::{CLONE, Does, HAND, MAX_CHILDREN, MindTool, Table, split};
use crate::window::{to_summarise, window};
use crate::{
    Block, Content, HandRecord, Item, Latest, MadeFrom, Message, MindSettings, ModelReply,
    ModelRequest, Outcome, Stop,
};

/// Something the host of a mind must do. A host performs a mind's outputs in
/// the order given, and makes each [`Output::Append`] and [`Output::Fork`]
/// durable before it performs what follows it.
#[derive(Clone, Debug, PartialEq)]
pub enum Output {
    /// Store this item at the end of the history.
    Append(Item),
    /// Send this to a hand, if it is connected. A hand that is not is told
    /// what it needs when it next says hello.
    Send {
        /// The hand.
        hand: Name,
        /// What to send it.
        message: FromMind,
    },
    /// End a hand's connection with this refusal, if it is connected.
    Close {
        /// The hand.
        hand: Name,
        /// Why.
        refusal: Refusal,
    },
    /// Ask the model, and answer with [`Mind::replied`] or [`Mind::failed`].
    Ask(ModelRequest),
    /// Make a child mind.
    Fork(Fork),
    /// Say `text` to the mind `to`, as a prompt from this one under `key`,
    /// and answer with [`Mind::told`]. The key makes a message said again,
    /// after a stop, the same prompt.
    Tell {
        /// The parent or child it is for.
        to: Name,
        /// The prompt's key.
        key: String,
        /// The message.
        text: String,
    },
    /// Answer with [`Mind::rested`] for each of these minds once it is at
    /// rest: at once for one that already is.
    Watch(Vec<Name>),
}

/// What the model is asked for a summary. A fact the user gave for later has
/// a heading of its own, as a model leaves out what no task has needed yet.
const SUMMARISE: &str = "\
This conversation is about to be replaced by a summary. Summarise it so that you can carry on \
from the summary alone, under these headings:

## Goal
What you were asked to do, with every requirement and constraint.

## Facts
Every name, value and fact you were given, exactly as given, whether or not it has been used yet.

## Progress
What is done and what you found.

## State
Exact file paths, commands, identifiers and unresolved errors.

## Next
The steps that remain, in order.

Keep every identifier and value exact and stay under 2000 words. Reply with the summary only.";

/// Why an interrupted mind is paused, as its history says it.
const INTERRUPTED: &str = "interrupted";

/// What a tool of the mind's own is told when its step forks.
const ALONE: &str = "a step that forks runs none of the mind's other tools; ask for this again \
                     after the fork";

/// What a mind is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Nothing, until it is prompted.
    Idle,
    /// It has a model to ask, or is asking it.
    Thinking,
    /// It has a call for a hand, or waits on one.
    Working,
    /// Nothing, ever again: it has ended.
    Ended,
}

impl State {
    /// Whether the mind is at rest: idle, or ended.
    pub fn rests(self) -> bool {
        matches!(self, Self::Idle | Self::Ended)
    }
}

/// What a mind is doing, told from its history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// What it is doing.
    pub state: State,
    /// How many items its history holds.
    pub length: u64,
    /// The hands a call or a tool use of its turn waits for, because they
    /// are not connected.
    pub waiting: Vec<Name>,
    /// The children a `wait` of its turn waits for.
    pub watching: Vec<Name>,
    /// What it said when it came to rest, while it is at rest.
    pub report: Option<Report>,
}

/// What a mind said when it last came to rest, as a parent that waits for
/// it is told.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Report {
    /// It ended its turn, and said this last.
    Said(String),
    /// It was stopped, for this reason, and waits for a prompt.
    Paused(String),
    /// Its model could not be asked, with this error, and it waits for a
    /// prompt.
    Failed(String),
    /// No mind has this name: a fork whose outcome is unknown may not have
    /// made it.
    Missing,
    /// It has ended.
    Ended,
}

/// What became of a prompt.
#[derive(Clone, Debug, PartialEq)]
pub struct Prompted {
    /// Whether the history already held this prompt under its key, so nothing
    /// was added.
    pub existing: bool,
    /// What the host must do.
    pub outputs: Vec<Output>,
}

/// Why a prompt is refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Conflict {
    /// The history already holds a prompt under its key, with other text.
    #[error("another prompt already has the key {key:?}")]
    Key {
        /// The key both prompts carry.
        key: String,
    },
    /// The mind has ended.
    #[error("the mind {0} has ended")]
    Ended(Name),
}

/// Why a message did not reach the mind it was for.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Untold {
    /// That mind has ended.
    #[error("{0} has ended")]
    Ended(Name),
    /// It could not be given the message, for this reason.
    #[error("{0}")]
    Failed(String),
}

/// A hand the mind does not have, or has released.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the mind has no hand named {0}")]
pub struct NoHand(pub Name);

/// Why a mind cannot be made from the first items of another's history.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Unbranched {
    /// The history holds fewer items.
    #[error("{mind}'s history ends at index {length}, before {at}")]
    Short {
        /// The mind whose history it is.
        mind: Name,
        /// How many items it holds.
        length: u64,
        /// How many were asked for.
        at: u64,
    },
    /// The mind had ended within those items.
    #[error("{mind} ended before index {at}: make a mind from fewer of its items")]
    Ended {
        /// The mind whose history it is.
        mind: Name,
        /// How many items were asked for.
        at: u64,
    },
    /// The mind had no such hand then.
    #[error("{mind} had no hand named {hand} at index {at}")]
    NoHand {
        /// The mind whose history it is.
        mind: Name,
        /// The hand asked for.
        hand: Name,
        /// How many items were asked for.
        at: u64,
    },
}

/// What a mind is told when it is made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// Its model, how hard it works, and the system prompt it is given.
    /// A model or effort left out is the host's.
    pub own: MindSettings,
    /// How many tokens a request may hold before the mind has its model sum
    /// the conversation up, and carries on from the summary and not from the
    /// beginning: what the last step read and wrote, and the results since.
    /// A model counts a token for every two to four bytes, and the mind
    /// counts one for every two, so it sums up early and never late. `None`
    /// never does, and a conversation that outgrows its model then ends in
    /// a model error.
    pub compact_at: Option<u64>,
    /// How many bytes of results the model is shown for one step, shared
    /// equally between the tools that step used. A result longer than its
    /// share is shown with its middle left out. The history keeps it whole.
    pub max_results: usize,
    /// How many clones the mind may have made with `clone_hand` and not
    /// released. Hands that join by themselves are not counted.
    pub max_clones: usize,
    /// How many minds a mind that starts from nothing may fork, its
    /// children's included: each fork shares what remains of it between the
    /// mind and its children. `None` sets no limit, and splits nothing:
    /// whether a fork is made is then for the hand it clones to say.
    pub max_forks: Option<u64>,
    /// Which of the mind's own tools its model is offered, each while it
    /// applies.
    pub tools: Vec<MindTool>,
}

impl Default for Settings {
    /// The host's model, no system prompt, a summary once a request would
    /// pass 150,000 tokens, 96 KB of results a step (40,000 tokens at the
    /// most), four clones, 64 forks, and every tool of the mind's own. The
    /// summary is asked for with the last step's results, so a model that
    /// reads 200,000 has room for it.
    fn default() -> Self {
        Self {
            own: MindSettings::default(),
            compact_at: Some(150_000),
            max_results: 96 * 1024,
            max_clones: 4,
            max_forks: Some(64),
            tools: MindTool::ALL.to_vec(),
        }
    }
}

/// One mind: a history, and the loop that extends it.
///
/// A mind holds the end of its history: everything, until a summary replaces
/// the conversation before it, and from then on only the items from the
/// index that summary names. What came before is in its host's store.
#[derive(Clone, Debug)]
pub struct Mind {
    name: Name,
    /// How many items of the history come before `items`.
    before: u64,
    items: Vec<Item>,
    settings: Settings,
    /// Every hand the history records, released ones included.
    hands: BTreeMap<Name, Hand>,
    /// Its parent, its children and its forks.
    kin: Kin,
    /// Whether it has ended.
    ended: bool,
    /// The request the model is answering now.
    asking: Option<Asking>,
    /// The message on its way to another mind.
    telling: Option<Telling>,
    /// The `wait` in progress, and what the minds it waits for have said.
    watching: Option<Watching>,
}

/// What a history records of the minds a mind is kin to.
#[derive(Clone, Debug, Default)]
struct Kin {
    parent: Option<Name>,
    /// In the order they were made.
    children: Vec<Name>,
    /// How many more minds it may fork, or none for no limit.
    forks_left: Option<u64>,
}

#[derive(Clone, Debug)]
struct Telling {
    tool_use: String,
    to: Name,
    key: String,
}

#[derive(Clone, Debug)]
struct Watching {
    tool_use: String,
    reports: BTreeMap<Name, Report>,
}

#[derive(Clone, Debug)]
struct Hand {
    record: HandRecord,
    attached: bool,
    /// Where the history first records it, or 0 for a hand a summary carries.
    joined: u64,
    /// Where it was released.
    released: Option<u64>,
}

#[derive(Clone, Debug)]
struct Asking {
    seen: u64,
    /// Whether the model was asked for a summary, not a step.
    summary: bool,
    /// The request's fingerprint, which the step it brings is stored with.
    under: String,
}

/// Where a clone is to go.
enum Target {
    /// Into this mind, as a hand of this name.
    Hand(Name),
    /// Into this many children of a fork.
    Children(usize),
}

enum Next {
    /// A tool use that does not run, for this reason.
    Skip {
        tool_use: String,
        why: String,
    },
    Issue {
        tool_use: String,
        hand: Name,
        capability: String,
        operation: String,
        input: Value,
    },
    Clone {
        tool_use: String,
        hand: Name,
        into: Target,
    },
    Release {
        tool_use: String,
        hand: Name,
    },
    Tell {
        tool_use: String,
        to: Name,
        key: String,
        text: String,
    },
    Watch {
        tool_use: String,
        minds: Vec<Name>,
    },
    Waited {
        tool_use: String,
        reports: BTreeMap<Name, Report>,
    },
    Await,
    Ask,
    Idle,
}

impl Mind {
    /// A mind named `name` with no history.
    pub fn new(name: Name, settings: Settings) -> Self {
        Self::load(name, 0, Vec::new(), settings)
    }

    /// A mind with this history, as stored: its items from index `from`,
    /// which is 0 for all of it, or else the index a store gives with a
    /// history's tail. It does nothing until it is [started](Mind::start),
    /// and asks no model until a hand says hello.
    pub fn load(name: Name, from: u64, items: Vec<Item>, settings: Settings) -> Self {
        let mut mind = Self::whole(name, &[], from, items, settings);
        mind.shed();
        mind
    }

    /// What the mind of a history is doing, told from what a store gives as
    /// its [`recent`](crate::Store::recent) items: the latest of it, and
    /// the items before them that say what it knows of its hands and kin. A
    /// host says this of a mind it has not loaded, which has no hand
    /// connected.
    pub fn status(name: Name, latest: Latest) -> Status {
        let Latest { known, from, items } = latest;
        Self::whole(name, &known, from, items, Settings::default()).status_now()
    }

    /// What the mind is doing.
    pub fn status_now(&self) -> Status {
        Status {
            state: self.state(),
            length: self.length(),
            waiting: self.waiting(),
            watching: self.watching(),
            report: self.report(),
        }
    }

    /// Does what a mind just loaded can before any hand says hello: say a
    /// message it was saying, watch the children it waits for, or ask its
    /// model once it has let every hand go.
    pub fn start(&mut self) -> Vec<Output> {
        let mut outputs = Vec::new();
        self.drive(&mut outputs);
        outputs
    }

    /// The mind's name.
    pub fn name(&self) -> &Name {
        &self.name
    }

    /// The items this mind holds: the end of its history.
    pub fn items(&self) -> &[Item] {
        &self.items
    }

    /// The index of the first item this mind holds. The items before it
    /// are in its host's store alone.
    pub fn starts_at(&self) -> u64 {
        self.before
    }

    /// How many items the history holds.
    pub fn length(&self) -> u64 {
        self.before + index(self.items.len())
    }

    /// The hands connected and settled, by name.
    pub fn connected(&self) -> Vec<Name> {
        let hands = self.hands.iter().filter(|(_, hand)| hand.attached);
        hands.map(|(name, _)| name.clone()).collect()
    }

    /// What the mind is doing.
    pub fn state(&self) -> State {
        if self.ended {
            return State::Ended;
        }
        match self.next() {
            Next::Idle => State::Idle,
            Next::Ask => State::Thinking,
            Next::Skip { .. }
            | Next::Issue { .. }
            | Next::Clone { .. }
            | Next::Release { .. }
            | Next::Tell { .. }
            | Next::Watch { .. }
            | Next::Waited { .. }
            | Next::Await => State::Working,
        }
    }

    /// The children a `wait` of the turn waits for: those that have not
    /// come to rest since it began.
    pub fn watching(&self) -> Vec<Name> {
        let Some((at, content)) = self.last_step() else {
            return Vec::new();
        };
        let table = self.table();
        for block in content {
            let Block::ToolUse { id, name, input } = block else {
                continue;
            };
            if self.outcome_of(id).is_some() {
                continue;
            }
            let (named, rest) = split(input);
            if let Some((Does::Mind(MindTool::Wait), _)) =
                self.meant(&table, name, named.as_ref(), at)
            {
                let heard = self
                    .watching
                    .as_ref()
                    .filter(|watching| watching.tool_use == *id)
                    .map(|watching| &watching.reports);
                let minds = self.waited_for(&rest).unwrap_or_default();
                return minds
                    .into_iter()
                    .filter(|mind| heard.is_none_or(|heard| !heard.contains_key(mind)))
                    .collect();
            }
        }
        Vec::new()
    }

    /// What the mind said when it came to rest, while it is at rest.
    pub fn report(&self) -> Option<Report> {
        match self.state() {
            State::Ended => return Some(Report::Ended),
            State::Thinking | State::Working => return None,
            State::Idle => {}
        }
        let last = self.items.iter().rev().find_map(|item| match item {
            Item::Step { content, .. } => {
                let said: Vec<&str> = content
                    .iter()
                    .filter_map(|block| match block {
                        Block::Text { text } => Some(text.as_str()),
                        Block::ToolUse { .. } | Block::Other { .. } => None,
                    })
                    .collect();
                Some(Report::Said(said.join("\n\n")))
            }
            Item::Paused { reason } => Some(Report::Paused(reason.clone())),
            Item::Failed { error } => Some(Report::Failed(error.clone())),
            _ => None,
        });
        Some(last.unwrap_or(Report::Said(String::new())))
    }

    /// The hands a call or a tool use of the turn waits for, because they
    /// are not connected.
    pub fn waiting(&self) -> Vec<Name> {
        let mut waiting = BTreeSet::new();
        for (_, hand, _) in self.pending() {
            if !self.is_attached(&hand) {
                waiting.insert(hand);
            }
        }
        if let Some((at, content)) = self.last_step() {
            let table = self.table();
            for block in content {
                let Block::ToolUse { id, name, input } = block else {
                    continue;
                };
                if self.outcome_of(id).is_some() || self.call_of(id).is_some() {
                    continue;
                }
                // The hand a use names, or else the one it would run on. A
                // mind told from its latest items alone may know no tools.
                let hand = match (split(input).0, table.find(name)) {
                    (Some(Ok(hand)), _) => Some(hand),
                    (_, Some((Does::Hand { .. }, holders))) => {
                        self.hand_for(None, at, holders, name).ok()
                    }
                    (_, Some((Does::Mind(_), cloners))) => {
                        let mut then = cloners.iter().filter(|hand| self.had(hand, at));
                        then.next().filter(|_| then.next().is_none()).cloned()
                    }
                    _ => None,
                };
                if let Some(hand) = hand.filter(|hand| !self.is_attached(hand)) {
                    waiting.insert(hand);
                }
            }
        }
        // A mind with its model to ask and every hand away waits for any.
        if waiting.is_empty() && matches!(self.next(), Next::Ask) && !self.may_ask() {
            let hands = self
                .hands
                .iter()
                .filter(|(_, hand)| hand.released.is_none());
            waiting.extend(hands.map(|(name, _)| name.clone()));
        }
        waiting.into_iter().collect()
    }

    /// Says something to the mind.
    ///
    /// A prompt that arrives mid-turn is stored at once and joins the turn at
    /// the next model call. A prompt under a key the history already holds
    /// adds nothing; if its text differs, it is a [`Conflict`].
    ///
    /// A mind knows the keys of the items it holds. For a mind that
    /// [starts](Mind::starts_at) past the beginning of its history, its
    /// host looks the key up in its store, and `earlier` is the text of the
    /// prompt it found there.
    ///
    /// `from` is the mind that says it, when its parent or one of its
    /// children does.
    ///
    /// A mind that has ended takes no prompt it does not hold already.
    pub fn prompt(
        &mut self,
        key: Option<String>,
        text: String,
        from: Option<Name>,
        earlier: Option<&str>,
    ) -> Result<Prompted, Conflict> {
        if let Some(key) = &key {
            let held = self.items.iter().find_map(|item| match item {
                Item::Prompt {
                    key: Some(held),
                    text,
                    ..
                } if held == key => Some(text.as_str()),
                _ => None,
            });
            match held.or(earlier) {
                Some(held) if held == text => {
                    return Ok(Prompted {
                        existing: true,
                        outputs: Vec::new(),
                    });
                }
                Some(_) => return Err(Conflict::Key { key: key.clone() }),
                None => {}
            }
        }
        if self.ended {
            return Err(Conflict::Ended(self.name.clone()));
        }
        let mut outputs = Vec::new();
        self.append(Item::Prompt { key, text, from }, &mut outputs);
        self.drive(&mut outputs);
        Ok(Prompted {
            existing: false,
            outputs,
        })
    }

    /// Takes a hand's `hello` and settles every call the mind waits on from
    /// that hand.
    ///
    /// Results the hand holds are stored, calls it may have run without a
    /// result are stored as unknown, and only then is anything sent: acks for
    /// what the hand holds, and again each call it never read.
    ///
    /// A turn interrupted while the hand was away is stopped now: a call the
    /// hand never read does not run, and one it still runs is cancelled.
    ///
    /// What the hand says it can do replaces what it said before, and the
    /// tools follow from the mind's next step.
    ///
    /// A hand the mind released is refused, and so is every hand of a mind
    /// that has ended.
    pub fn attach(&mut self, hello: &Hello) -> Result<Vec<Output>, Refusal> {
        if let Some(refusal) = self.refusal(hello) {
            return Err(refusal);
        }
        let hand = hello.hand.clone();
        let pending: Vec<(CallId, String)> = self
            .pending()
            .into_iter()
            .filter(|(_, of, _)| *of == hand)
            .map(|(id, _, tool_use)| (id, tool_use))
            .collect();
        let ids: Vec<CallId> = pending.iter().map(|(id, _)| *id).collect();
        let recorded = self.hands.get(&hand).map(|known| known.record.boot.clone());
        let plan = settle(recorded.as_deref(), &ids, hello);
        let interrupted = self
            .round()
            .iter()
            .any(|item| matches!(item, Item::Paused { reason } if reason == INTERRUPTED));

        let mut outputs = Vec::new();
        let mut resend = Vec::new();
        let mut cancel = Vec::new();
        for ((id, tool_use), (_, settled)) in pending.into_iter().zip(plan.calls) {
            let outcome = match settled {
                Settled::Take(reply) => {
                    self.took(id, tool_use, reply, &mut outputs);
                    continue;
                }
                Settled::Unknown if plan.new_boot => {
                    let why = "the hand restarted after it was sent this call, and remembers nothing of it";
                    self.unknown(id, tool_use, why, &mut outputs);
                    continue;
                }
                Settled::Unknown => {
                    let why = "the hand read this call but holds no result for it";
                    self.unknown(id, tool_use, why, &mut outputs);
                    continue;
                }
                Settled::Wait => {
                    if interrupted {
                        cancel.push(id);
                    }
                    continue;
                }
                Settled::Resend if interrupted => Outcome::NotRun(
                    "the turn was interrupted before the hand read this call".into(),
                ),
                Settled::Resend => {
                    resend.push(id);
                    continue;
                }
            };
            let item = Item::Outcome {
                tool_use,
                call: Some(id),
                outcome,
            };
            self.append(item, &mut outputs);
        }
        let can_else = self
            .hands
            .get(&hand)
            .is_some_and(|known| known.record.capabilities != hello.capabilities);
        if plan.new_boot || can_else {
            let item = Item::Attached {
                hand: hand.clone(),
                boot: hello.boot.clone(),
                capabilities: hello.capabilities.clone(),
            };
            self.append(item, &mut outputs);
        }
        if let Some(known) = self.hands.get_mut(&hand) {
            known.attached = true;
        }

        let send = |message| Output::Send {
            hand: hand.clone(),
            message,
        };
        if !plan.ack.is_empty() {
            outputs.push(send(FromMind::Ack { ids: plan.ack }));
        }
        for id in resend {
            if let Some(call) = self.call(id) {
                outputs.push(send(FromMind::Call(call)));
            }
        }
        for id in cancel {
            outputs.push(send(FromMind::Cancel { id }));
        }
        self.drive(&mut outputs);
        Ok(outputs)
    }

    /// Why a hand that says `hello` is refused, if it is.
    fn refusal(&self, hello: &Hello) -> Option<Refusal> {
        // Every tool's `hand` names the hand that runs it, so an operation
        // that takes one of its own could never be given it.
        let mut operations = hello
            .capabilities
            .iter()
            .flat_map(|capability| &capability.operations);
        if operations.any(|operation| operation.input["properties"].get(HAND).is_some()) {
            return Some(Refusal::Protocol);
        }
        let released = self
            .hands
            .get(&hello.hand)
            .is_some_and(|known| known.released.is_some());
        (self.ended || released).then_some(Refusal::Released)
    }

    /// A hand's connection dropped. The mind sends it nothing until it says
    /// hello again.
    pub fn detach(&mut self, hand: &Name) {
        if let Some(known) = self.hands.get_mut(hand) {
            known.attached = false;
        }
    }

    /// Takes a result from a hand. The first result for a call sent to that
    /// hand is stored and acked; any other is only acked.
    pub fn result(&mut self, hand: &Name, result: CallResult) -> Vec<Output> {
        let mut outputs = Vec::new();
        let waiting = self
            .pending()
            .into_iter()
            .find(|(id, of, _)| *id == result.id && of == hand);
        if let Some((id, _, tool_use)) = waiting {
            self.took(id, tool_use, result.reply, &mut outputs);
        }
        outputs.push(Output::Send {
            hand: hand.clone(),
            message: FromMind::Ack {
                ids: vec![result.id],
            },
        });
        self.drive(&mut outputs);
        outputs
    }

    /// Takes the model's reply to the request that carried `seen`. A reply
    /// to any other request, such as one an interrupt gave up on, is dropped.
    pub fn replied(&mut self, seen: u64, reply: ModelReply) -> Vec<Output> {
        let Some(asking) = self.asking.take_if(|asking| asking.seen == seen) else {
            return Vec::new();
        };
        let ModelReply {
            content,
            stop,
            usage,
        } = reply;
        let item = if asking.summary {
            let said: Vec<&str> = content
                .iter()
                .filter_map(|block| match block {
                    Block::Text { text } => Some(text.as_str()),
                    Block::ToolUse { .. } | Block::Other { .. } => None,
                })
                .collect();
            let text = said.join("\n\n");
            if text.trim().is_empty() {
                Item::Failed {
                    error: "the model wrote no summary of the conversation".into(),
                }
            } else {
                // A mind carries on from the last step's prompts, the hands
                // the history recorded and the last call it made.
                let from = match self.round().first() {
                    Some(Item::Step { seen, .. }) => *seen,
                    _ => self.length(),
                };
                Item::Summary {
                    text,
                    usage,
                    from,
                    hands: self
                        .hands
                        .iter()
                        .map(|(name, hand)| (name.clone(), hand.record.clone()))
                        .collect(),
                    last_call: self.last_call(),
                    forks_left: self.kin.forks_left,
                    parent: self.kin.parent.clone(),
                    children: self.kin.children.clone(),
                }
            }
        } else {
            Item::Step {
                seen,
                content,
                stop,
                usage,
                under: Some(asking.under),
            }
        };
        let mut outputs = Vec::new();
        self.append(item, &mut outputs);
        self.shed();
        self.drive(&mut outputs);
        outputs
    }

    /// The model could not answer the request that carried `seen`. The mind
    /// stops until it is prompted.
    pub fn failed(&mut self, seen: u64, error: impl Into<String>) -> Vec<Output> {
        if self.asking.take_if(|asking| asking.seen == seen).is_none() {
            return Vec::new();
        }
        let mut outputs = Vec::new();
        self.append(
            Item::Failed {
                error: error.into(),
            },
            &mut outputs,
        );
        outputs
    }

    /// Stops the turn.
    ///
    /// The reply to a request the model is still answering is dropped, tool
    /// uses that had not become calls do not run, and every hand is told to
    /// cancel every call it runs. The mind is then paused: it stores what
    /// its hands return for those calls, and asks no model until it is
    /// prompted. A hand that is away is told when it next says hello. A mind
    /// with nothing to stop does nothing.
    pub fn interrupt(&mut self) -> Vec<Output> {
        let mut outputs = Vec::new();
        if self.asking.take().is_none() && matches!(self.next(), Next::Idle) {
            return outputs;
        }
        self.watching = None;
        // A message on its way may have arrived: its outcome is what the
        // mind it went to says.
        let telling = self
            .telling
            .as_ref()
            .map(|telling| telling.tool_use.clone());
        for tool_use in self.unissued() {
            if telling.as_ref() == Some(&tool_use) {
                continue;
            }
            let outcome =
                Outcome::NotRun("the turn was interrupted before this call started".into());
            self.append(
                Item::Outcome {
                    tool_use,
                    call: None,
                    outcome,
                },
                &mut outputs,
            );
        }
        self.append(
            Item::Paused {
                reason: INTERRUPTED.into(),
            },
            &mut outputs,
        );
        for (id, hand, _) in self.pending() {
            outputs.push(Output::Send {
                hand,
                message: FromMind::Cancel { id },
            });
        }
        outputs
    }

    /// The mind a message went to has it, or could not be given it. `key`
    /// is the one the [`Output::Tell`] carried. A message to a mind that
    /// has ended did not run.
    pub fn told(&mut self, key: &str, result: Result<(), Untold>) -> Vec<Output> {
        let mut outputs = Vec::new();
        let Some(telling) = self.telling.take_if(|telling| telling.key == key) else {
            return outputs;
        };
        if self.outcome_of(&telling.tool_use).is_none() {
            let outcome = match result {
                Ok(()) => Outcome::Output(json!({ "told": telling.to })),
                Err(ended @ Untold::Ended(_)) => Outcome::NotRun(ended.to_string()),
                Err(Untold::Failed(error)) => Outcome::Error(error),
            };
            let item = Item::Outcome {
                tool_use: telling.tool_use,
                call: None,
                outcome,
            };
            self.append(item, &mut outputs);
        }
        self.drive(&mut outputs);
        outputs
    }

    /// A mind this one watches is at rest, and said `report`.
    pub fn rested(&mut self, mind: &Name, report: Report) -> Vec<Output> {
        let mut outputs = Vec::new();
        if let Some(watching) = &mut self.watching {
            watching.reports.insert(mind.clone(), report);
            self.drive(&mut outputs);
        }
        outputs
    }

    /// Lets a hand go from outside the conversation: a hand that died while
    /// it ran a call is how a turn would otherwise wait for ever. Every call
    /// it was sent and did not answer is stored as unknown, and the hand is
    /// refused from now on.
    pub fn release(&mut self, hand: &Name) -> Result<Vec<Output>, NoHand> {
        if !self.is_known(hand) {
            return Err(NoHand(hand.clone()));
        }
        let mut outputs = Vec::new();
        self.release_hand(
            hand,
            "it was released from outside the conversation",
            &mut outputs,
        );
        self.drive(&mut outputs);
        Ok(outputs)
    }

    /// Ends the mind, from outside its conversation: its turn stops, every
    /// hand is let go, and from then on it asks its model nothing and takes
    /// no prompt. A tool use that had not started does not run, a call a
    /// hand was sent and did not answer is unknown, and a message on its
    /// way may have arrived. Its history stays, for the minds that share
    /// it. A mind that has ended does nothing.
    pub fn end(&mut self) -> Vec<Output> {
        let mut outputs = Vec::new();
        if self.ended {
            return outputs;
        }
        self.asking = None;
        self.watching = None;
        if let Some(telling) = self.telling.take()
            && self.outcome_of(&telling.tool_use).is_none()
        {
            let why = format!(
                "the mind ended before it heard whether {} had the message",
                telling.to
            );
            let item = Item::Outcome {
                tool_use: telling.tool_use,
                call: None,
                outcome: Outcome::Unknown(why),
            };
            self.append(item, &mut outputs);
        }
        for tool_use in self.unissued() {
            let outcome = Outcome::NotRun("the mind ended before this call started".into());
            let item = Item::Outcome {
                tool_use,
                call: None,
                outcome,
            };
            self.append(item, &mut outputs);
        }
        let hands: Vec<Name> = self
            .hands
            .iter()
            .filter(|(_, hand)| hand.released.is_none())
            .map(|(name, _)| name.clone())
            .collect();
        for hand in hands {
            self.release_hand(&hand, "the mind ended", &mut outputs);
        }
        self.append(Item::Ended, &mut outputs);
        outputs
    }

    /// The first items of a mind made `from` another's history: the first
    /// `at` items of `from.mind`'s, keeping its hand `from.hand`. `tail` is
    /// that history's end as it was then, which a store's
    /// [`tail_at`](crate::Store::tail_at) gives, and `last_call` the
    /// highest id of a call `from.mind` has made by now.
    ///
    /// The new mind keeps `hand`, which says hello as it again. A clone in
    /// flight at `at` is `from.mind`'s: its outcome is unknown, on any
    /// hand. Every other hand stays with `from.mind`: each is let go, and a
    /// call it had not answered is unknown. Then the mind leaves
    /// `from.mind`'s kin: it has no parent and no children, forks from
    /// `settings` as one that starts from nothing, and numbers its calls
    /// above `last_call`, so that a hand restored from later than `at`
    /// reads them as new.
    pub fn branch(
        from: &MadeFrom,
        (start, items): (u64, Vec<Item>),
        last_call: CallId,
        settings: &Settings,
    ) -> Result<Vec<Item>, Unbranched> {
        let MadeFrom { mind: of, at, hand } = from;
        let length = start + index(items.len());
        if length < *at {
            return Err(Unbranched::Short {
                mind: of.clone(),
                length,
                at: *at,
            });
        }
        let mut mind = Self::whole(of.clone(), &[], start, items, settings.clone());
        if mind.ended {
            return Err(Unbranched::Ended {
                mind: of.clone(),
                at: *at,
            });
        }
        if !mind.is_known(hand) {
            return Err(Unbranched::NoHand {
                mind: of.clone(),
                hand: hand.clone(),
                at: *at,
            });
        }
        let mut outputs = Vec::new();
        let clones: Vec<(CallId, String)> = mind
            .round()
            .iter()
            .filter_map(|item| match item {
                Item::Call {
                    id,
                    tool_use,
                    capability,
                    ..
                } if capability == CLONE && mind.outcome_of(tool_use).is_none() => {
                    Some((*id, tool_use.clone()))
                }
                _ => None,
            })
            .collect();
        for (id, tool_use) in clones {
            let why = format!(
                "{of} was cloning a hand when this mind was made from it: the clone is {of}'s"
            );
            let item = Item::Outcome {
                tool_use,
                call: Some(id),
                outcome: Outcome::Unknown(why),
            };
            mind.append(item, &mut outputs);
        }
        let others: Vec<Name> = mind
            .hands
            .iter()
            .filter(|(name, known)| *name != hand && known.released.is_none())
            .map(|(name, _)| name.clone())
            .collect();
        let why = format!("it stayed with {of} when this mind was made from it");
        for other in others {
            mind.release_hand(&other, &why, &mut outputs);
        }
        let item = Item::Branched {
            from: of.clone(),
            at: *at,
            left: settings.max_forks,
            last_call: last_call.max(mind.last_call()),
        };
        mind.append(item, &mut outputs);
        Ok(outputs
            .into_iter()
            .filter_map(|output| match output {
                Output::Append(item) => Some(item),
                _ => None,
            })
            .collect())
    }

    /// A mind that holds `items`, from index `from`, with nothing shed. It
    /// first learns what `known` says, each item at its index: what came
    /// before `items`, as far as it tells of the hands and the kin.
    fn whole(
        name: Name,
        known: &[(u64, Item)],
        from: u64,
        items: Vec<Item>,
        settings: Settings,
    ) -> Self {
        let mut mind = Self {
            name,
            before: from,
            items,
            kin: Kin {
                forks_left: settings.max_forks,
                ..Kin::default()
            },
            ended: false,
            settings,
            hands: BTreeMap::new(),
            asking: None,
            telling: None,
            watching: None,
        };
        for (at, item) in known {
            note(&mut mind.hands, &mut mind.kin, &mut mind.ended, item, *at);
        }
        // What the last summary carries, then what came after it.
        let summary = mind
            .items
            .iter()
            .rposition(|item| matches!(item, Item::Summary { .. }));
        for (at, item) in (mind.before..).zip(&mind.items).skip(summary.unwrap_or(0)) {
            note(&mut mind.hands, &mut mind.kin, &mut mind.ended, item, at);
        }
        mind
    }

    /// Forgets what the last summary replaced: every item before the index
    /// it names.
    fn shed(&mut self) {
        let from = self.items.iter().rev().find_map(|item| match item {
            Item::Summary { from, .. } => Some(*from),
            _ => None,
        });
        let Some(from) = from else {
            return;
        };
        let shed = self.local(from).min(self.items.len());
        self.items.drain(..shed);
        // The room the shed items took is given up with them.
        self.items.shrink_to_fit();
        self.before += index(shed);
    }

    /// Where in the items this mind holds the item at `at` in the history
    /// is, or 0 for one that came before them.
    fn local(&self, at: u64) -> usize {
        usize::try_from(at.saturating_sub(self.before)).unwrap_or(usize::MAX)
    }

    fn append(&mut self, item: Item, outputs: &mut Vec<Output>) {
        let at = self.length();
        note(&mut self.hands, &mut self.kin, &mut self.ended, &item, at);
        self.items.push(item.clone());
        outputs.push(Output::Append(item));
    }

    /// Does everything the history allows, up to the next thing to wait for.
    fn drive(&mut self, outputs: &mut Vec<Output>) {
        loop {
            match self.next() {
                Next::Skip { tool_use, why } => {
                    let item = Item::Outcome {
                        tool_use,
                        call: None,
                        outcome: Outcome::NotRun(why),
                    };
                    self.append(item, outputs);
                }
                Next::Issue {
                    tool_use,
                    hand,
                    capability,
                    operation,
                    input,
                } => self.issue(tool_use, hand, capability, operation, input, outputs),
                Next::Clone {
                    tool_use,
                    hand,
                    into,
                } => {
                    let id = self.last_call() + 1;
                    let into: Vec<Value> = match into {
                        Target::Hand(clone) => vec![json!({"mind": self.name, "hand": clone})],
                        Target::Children(count) => (0..count)
                            .map_while(|child| child_name(&self.name, id, child))
                            .map(|child| json!({"mind": child, "hand": hand}))
                            .collect(),
                    };
                    let input = json!({ "into": into });
                    self.issue(tool_use, hand, CLONE.into(), CLONE.into(), input, outputs);
                }
                Next::Release { tool_use, hand } => {
                    self.release_hand(&hand, "the model released it", outputs);
                    let item = Item::Outcome {
                        tool_use,
                        call: None,
                        outcome: Outcome::Output(json!({ "released": hand })),
                    };
                    self.append(item, outputs);
                }
                Next::Tell {
                    tool_use,
                    to,
                    key,
                    text,
                } => {
                    self.telling = Some(Telling {
                        tool_use,
                        to: to.clone(),
                        key: key.clone(),
                    });
                    outputs.push(Output::Tell { to, key, text });
                }
                Next::Watch { tool_use, minds } => {
                    self.watching = Some(Watching {
                        tool_use,
                        reports: BTreeMap::new(),
                    });
                    outputs.push(Output::Watch(minds));
                }
                Next::Waited { tool_use, reports } => {
                    self.watching = None;
                    let item = Item::Outcome {
                        tool_use,
                        call: None,
                        outcome: Outcome::Output(json!(reports)),
                    };
                    self.append(item, outputs);
                }
                Next::Ask if self.asking.is_none() && self.may_ask() => {
                    let (request, under) = self.request();
                    self.asking = Some(Asking {
                        seen: request.seen,
                        summary: request.text_only,
                        under,
                    });
                    outputs.push(Output::Ask(request));
                    return;
                }
                Next::Await | Next::Ask | Next::Idle => return,
            }
        }
    }

    fn issue(
        &mut self,
        tool_use: String,
        hand: Name,
        capability: String,
        operation: String,
        input: Value,
        outputs: &mut Vec<Output>,
    ) {
        let call = Call {
            id: self.last_call() + 1,
            capability,
            operation,
            input,
        };
        let stored = Item::Call {
            id: call.id,
            hand: hand.clone(),
            tool_use,
            capability: call.capability.clone(),
            operation: call.operation.clone(),
            input: call.input.clone(),
        };
        self.append(stored, outputs);
        outputs.push(Output::Send {
            hand,
            message: FromMind::Call(call),
        });
    }

    /// Stores how a call ended. A clone that the mind's own tools asked for
    /// is told to the model in their terms, and the children of a fork are
    /// made before the fork is stored.
    fn took(&mut self, id: CallId, tool_use: String, reply: Reply, outputs: &mut Vec<Output>) {
        let clone = self.round().iter().find_map(|item| match item {
            Item::Call {
                id: of,
                hand,
                capability,
                input,
                ..
            } if *of == id && capability == CLONE => Some((hand.clone(), input.clone())),
            _ => None,
        });
        let outcome = match (clone, reply) {
            (Some((hand, input)), Reply::Output(_)) => {
                let into: Vec<(Name, Name)> = input["into"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|target| {
                        let name = |key: &str| Name::new(target[key].as_str()?).ok();
                        Some((name("mind")?, name("hand")?))
                    })
                    .collect();
                if into.iter().all(|(mind, _)| *mind == self.name) {
                    // The clone is this mind's hand from now on: tool uses
                    // for it wait until it says hello.
                    for (_, clone) in &into {
                        let item = Item::Cloned {
                            hand: clone.clone(),
                            from: hand.clone(),
                        };
                        self.append(item, outputs);
                    }
                    let clone = into.first().map(|(_, clone)| clone);
                    Outcome::Output(json!({ "hand": clone }))
                } else {
                    let children: Vec<Name> = into
                        .into_iter()
                        .map(|(mind, _)| mind)
                        .filter(|mind| self.is_own_child(mind))
                        .collect();
                    if children.is_empty() {
                        Outcome::Unknown("the clone was for minds that are not this one's".into())
                    } else {
                        self.forked(id, &tool_use, &hand, &children, outputs);
                        return;
                    }
                }
            }
            (_, reply) => Outcome::from(reply),
        };
        let item = Item::Outcome {
            tool_use,
            call: Some(id),
            outcome,
        };
        self.append(item, outputs);
    }

    /// A fork's clones exist: its children are made, then the fork stored.
    fn forked(
        &mut self,
        id: CallId,
        tool_use: &str,
        hand: &Name,
        children: &[Name],
        outputs: &mut Vec<Output>,
    ) {
        let prompts: Vec<String> = self
            .last_step()
            .into_iter()
            .flat_map(|(_, content)| content)
            .find_map(|block| match block {
                Block::ToolUse { id: of, input, .. } if of == tool_use => {
                    input["prompts"].as_array().map(|prompts| {
                        let prompts = prompts.iter().filter_map(Value::as_str);
                        prompts.map(str::to_owned).collect()
                    })
                }
                _ => None,
            })
            .unwrap_or_default();
        let at = self
            .items
            .iter()
            .rposition(|item| matches!(item, Item::Call { id: of, .. } if *of == id));
        let inherited = self.before + index(at.map_or(self.items.len(), |at| at + 1));
        let (share, kept) = shares(self.kin.forks_left, index(children.len()));
        // The hands the child shares the history of, as they were at the
        // clone call: every one but the cloned stays with the parent.
        let others: Vec<Name> = self
            .hands
            .iter()
            .filter(|(name, known)| {
                *name != hand
                    && known.joined < inherited
                    && known.released.is_none_or(|released| released >= inherited)
            })
            .map(|(name, _)| name.clone())
            .collect();
        for (child, prompt) in children.iter().zip(prompts) {
            let mut items = vec![Item::Outcome {
                tool_use: tool_use.to_owned(),
                call: Some(id),
                outcome: Outcome::Output(json!({"you": child, "parent": self.name})),
            }];
            items.extend(others.iter().map(|other| Item::Released {
                hand: other.clone(),
                reason: format!(
                    "it stayed with {} when this mind was forked from it",
                    self.name
                ),
            }));
            items.push(Item::Parted {
                parent: self.name.clone(),
                left: share,
            });
            items.push(Item::Prompt {
                key: None,
                text: prompt,
                from: Some(self.name.clone()),
            });
            outputs.push(Output::Fork(Fork {
                name: child.clone(),
                parent: self.name.clone(),
                inherited,
                items,
            }));
        }
        let item = Item::Outcome {
            tool_use: tool_use.to_owned(),
            call: Some(id),
            outcome: Outcome::Output(json!({ "children": children })),
        };
        self.append(item, outputs);
        let item = Item::Forked {
            children: children.to_vec(),
            left: kept,
        };
        self.append(item, outputs);
    }

    /// Stores a call's outcome as unknown. A clone that may have been made
    /// is told as such: a fork's children may exist, so the model is told
    /// their names, and their share of the budget is spent.
    fn unknown(&mut self, id: CallId, tool_use: String, why: &str, outputs: &mut Vec<Output>) {
        let into = self.round().iter().find_map(|item| match item {
            Item::Call {
                id: of,
                capability,
                input,
                ..
            } if *of == id && capability == CLONE => Some(input["into"].clone()),
            _ => None,
        });
        let children: Vec<Name> = into
            .iter()
            .flat_map(|into| into.as_array().into_iter().flatten())
            .filter_map(|target| Name::new(target["mind"].as_str()?).ok())
            .filter(|mind| self.is_own_child(mind))
            .collect();
        let outcome = if children.is_empty() {
            Outcome::Unknown(why.into())
        } else {
            Outcome::Unknown(format!(
                "{why}; it may have made the child minds {}, which then work on their own",
                join(&children)
            ))
        };
        let item = Item::Outcome {
            tool_use,
            call: Some(id),
            outcome,
        };
        self.append(item, outputs);
        if !children.is_empty() {
            let (_, kept) = shares(self.kin.forks_left, index(children.len()));
            let item = Item::Forked {
                children,
                left: kept,
            };
            self.append(item, outputs);
        }
    }

    fn release_hand(&mut self, hand: &Name, reason: &str, outputs: &mut Vec<Output>) {
        for (id, of, tool_use) in self.pending() {
            if of == *hand {
                self.unknown(
                    id,
                    tool_use,
                    "the hand was released before it answered",
                    outputs,
                );
            }
        }
        let item = Item::Released {
            hand: hand.clone(),
            reason: reason.into(),
        };
        self.append(item, outputs);
        outputs.push(Output::Close {
            hand: hand.clone(),
            refusal: Refusal::Released,
        });
    }

    fn next(&self) -> Next {
        // A mind that has ended does nothing more.
        if self.ended {
            return Next::Idle;
        }
        let step = self
            .items
            .iter()
            .enumerate()
            .rev()
            .find_map(|(at, item)| match item {
                Item::Step {
                    seen,
                    content,
                    stop,
                    ..
                } => Some((at, *seen, content, *stop)),
                _ => None,
            });
        if let Some((at, _, content, stop)) = step
            && let Some(next) = self.next_in(at, content, stop)
        {
            return next;
        }

        // The round is closed. A pause or a failure after the last step holds
        // the mind until a prompt follows it.
        let round_at = step.map_or(0, |(at, ..)| at);
        let gate = self.items[round_at..]
            .iter()
            .rposition(|item| matches!(item, Item::Paused { .. } | Item::Failed { .. }))
            .map(|gate| round_at + gate);
        let unseen_from = match (gate, step) {
            (Some(gate), _) => gate + 1,
            (None, Some((_, seen, ..))) => self.local(seen),
            (None, None) => 0,
        };
        let prompted = self
            .items
            .iter()
            .skip(unseen_from)
            .any(|item| matches!(item, Item::Prompt { .. }));
        let owes_results = gate.is_none()
            && step.is_some_and(|(_, _, content, _)| {
                content
                    .iter()
                    .any(|block| matches!(block, Block::ToolUse { .. }))
            });
        if prompted || owes_results {
            Next::Ask
        } else {
            Next::Idle
        }
    }

    /// What the tool uses of the step at `at` call for next, or nothing
    /// once every one of them has ended.
    fn next_in(&self, at: usize, content: &[Block], stop: Stop) -> Option<Next> {
        let table = self.table();
        // Hands an earlier tool use of this step still waits on.
        let mut busy = BTreeSet::new();
        let mut waiting = false;
        // The first of the mind's own tools that has not ended. The others
        // wait their turn.
        let mut own = None;
        // A fork runs alone among the mind's own tools: one that has not
        // ended, or is a call.
        let fork = content.iter().find_map(|block| match block {
            Block::ToolUse { id, name, input }
                if self.outcome_of(id).is_none() || self.call_of(id).is_some() =>
            {
                let named = split(input).0;
                let meant = self.meant(&table, name, named.as_ref(), at);
                matches!(meant, Some((Does::Mind(MindTool::Fork), _))).then_some(id)
            }
            Block::ToolUse { .. } | Block::Text { .. } | Block::Other { .. } => None,
        });
        for block in content {
            let Block::ToolUse { id, name, input } = block else {
                continue;
            };
            if self.outcome_of(id).is_some() {
                continue;
            }
            // A use that became a call is the call's, whatever the tools are
            // called now: it waits for its result on its hand.
            if let Some(hand) = self.called(id) {
                busy.insert(hand);
                waiting = true;
                continue;
            }
            let skip = |why: String| {
                Some(Next::Skip {
                    tool_use: id.clone(),
                    why,
                })
            };
            if stop != Stop::ToolUse {
                return skip("the model stopped before it finished asking for this call".into());
            }
            let (named, rest) = split(input);
            let Some((does, holders)) = self.meant(&table, name, named.as_ref(), at) else {
                return skip(not_meant(&table, name));
            };
            match does {
                Does::Mind(_) if fork.is_some_and(|fork| fork != id) => {
                    return skip(ALONE.into());
                }
                Does::Mind(tool) => {
                    if own.is_none() {
                        own = Some((id, *tool, input));
                    }
                }
                Does::Hand {
                    capability,
                    operation,
                } => {
                    let hand = match self.hand_for(named, at, holders, name) {
                        Ok(hand) => hand,
                        Err(why) => return skip(why),
                    };
                    if busy.contains(&hand) || !self.is_attached(&hand) {
                        busy.insert(hand);
                        waiting = true;
                        continue;
                    }
                    return Some(Next::Issue {
                        tool_use: id.clone(),
                        hand,
                        capability: capability.clone(),
                        operation: operation.clone(),
                        input: rest,
                    });
                }
            }
        }
        // The mind's own tools run once every hand tool of their step has
        // ended, so nothing runs on any hand while one is cloned, and one at
        // a time.
        match own {
            _ if waiting => Some(Next::Await),
            Some((id, tool, input)) => Some(self.own(id, tool, input, at, &table)),
            None => None,
        }
    }

    /// What the mind's own tool `tool`, used as `id` with `input` in the
    /// step at `step_at`, does next.
    fn own(&self, id: &str, tool: MindTool, input: &Value, step_at: usize, table: &Table) -> Next {
        let skip = |why: String| Next::Skip {
            tool_use: id.to_owned(),
            why,
        };
        let (named, rest) = split(input);
        match tool {
            MindTool::Tell => return self.tell(id, &rest, step_at),
            MindTool::Wait => return self.wait(id, &rest),
            MindTool::Fork | MindTool::CloneHand | MindTool::ReleaseHand => {}
        }
        if tool == MindTool::ReleaseHand {
            return match named {
                Some(Ok(hand)) if self.is_known(&hand) => Next::Release {
                    tool_use: id.to_owned(),
                    hand,
                },
                Some(Ok(hand)) => skip(format!(
                    "there is no hand named {hand}. {}",
                    self.hands_line()
                )),
                _ => skip(format!("say which hand to let go. {}", self.hands_line())),
            };
        }
        let cloners = table
            .find(tool.name())
            .map_or(&[][..], |(_, cloners)| cloners);
        let listed = join(cloners);
        let hand = match named {
            Some(Ok(hand)) => hand,
            Some(Err(bad)) => {
                return skip(format!(
                    "{bad} is not the name of a hand. {}",
                    self.hands_line()
                ));
            }
            None => {
                let mut then = cloners.iter().filter(|hand| self.had(hand, step_at));
                match (then.next(), then.next()) {
                    (Some(only), None) => only.clone(),
                    _ => return skip(format!("say which hand to clone: {listed}")),
                }
            }
        };
        if !cloners.contains(&hand) {
            return skip(format!(
                "{hand} cannot be cloned. These hands can: {listed}."
            ));
        }
        let into = if tool == MindTool::Fork {
            let prompts = rest["prompts"].as_array();
            let prompts = prompts.filter(|prompts| prompts.iter().all(Value::is_string));
            let count = prompts
                .map(Vec::len)
                .filter(|count| (1..=MAX_CHILDREN).contains(count));
            let Some(count) = count else {
                return skip(format!(
                    "prompts are 1 to {MAX_CHILDREN} strings, one per child"
                ));
            };
            if let Some(left) = self.kin.forks_left
                && index(count) > left
            {
                return skip(format!(
                    "this mind may fork {left} more minds, and this asked for {count}"
                ));
            }
            Target::Children(count)
        } else {
            if self.clones() >= self.settings.max_clones {
                return skip(format!(
                    "this mind has all the clones it may: {}",
                    self.settings.max_clones
                ));
            }
            let clone = match &rest["name"] {
                Value::Null => self.clone_name(&hand),
                Value::String(name) => Name::new(name.as_str()).ok(),
                _ => None,
            };
            match clone {
                Some(clone) if !self.hands.contains_key(&clone) => Target::Hand(clone),
                Some(clone) => return skip(format!("a hand is already named {clone}")),
                None => return skip("that cannot name a hand".into()),
            }
        };
        if !self.is_attached(&hand) {
            return Next::Await;
        }
        Next::Clone {
            tool_use: id.to_owned(),
            hand,
            into,
        }
    }

    /// What `tell`, used as `id` with `input` in the step at `step_at`,
    /// does next.
    fn tell(&self, id: &str, input: &Value, step_at: usize) -> Next {
        let skip = |why: String| Next::Skip {
            tool_use: id.to_owned(),
            why,
        };
        if self.kin.parent.is_none() && self.kin.children.is_empty() {
            return skip("this mind has no parent or children to tell".into());
        }
        let to = input["mind"].as_str().and_then(|to| Name::new(to).ok());
        let Some(to) = to.filter(|to| self.is_kin(to)) else {
            return skip(format!(
                "say which mind to tell: your parent or one of your children. {}",
                self.kin_line()
            ));
        };
        let Some(text) = input["text"]
            .as_str()
            .filter(|text| !text.trim().is_empty())
        else {
            return skip("say what to tell them".into());
        };
        // The same message is the same prompt, however often it is said.
        let key = format!("{}/{}/{id}", self.name, self.before + index(step_at));
        if self
            .telling
            .as_ref()
            .is_some_and(|telling| telling.key == key)
        {
            return Next::Await;
        }
        Next::Tell {
            tool_use: id.to_owned(),
            to,
            key,
            text: text.to_owned(),
        }
    }

    /// What `wait`, used as `id` with `input`, does next.
    fn wait(&self, id: &str, input: &Value) -> Next {
        let minds = match self.waited_for(input) {
            Ok(minds) => minds,
            Err(why) => {
                return Next::Skip {
                    tool_use: id.to_owned(),
                    why,
                };
            }
        };
        let Some(watching) = self
            .watching
            .as_ref()
            .filter(|watching| watching.tool_use == id)
        else {
            return Next::Watch {
                tool_use: id.to_owned(),
                minds,
            };
        };
        let reports: Option<BTreeMap<Name, Report>> = minds
            .into_iter()
            .map(|mind| {
                let report = watching.reports.get(&mind)?.clone();
                Some((mind, report))
            })
            .collect();
        match reports {
            Some(reports) => Next::Waited {
                tool_use: id.to_owned(),
                reports,
            },
            None => Next::Await,
        }
    }

    /// The children a `wait` with `input` is for: those it names, or all.
    fn waited_for(&self, input: &Value) -> Result<Vec<Name>, String> {
        if self.kin.children.is_empty() {
            return Err("this mind has no children to wait for".into());
        }
        let minds = match &input["minds"] {
            Value::Null => self.kin.children.clone(),
            Value::Array(minds) => {
                let mut named = Vec::new();
                for mind in minds {
                    let mind = mind.as_str().and_then(|mind| Name::new(mind).ok());
                    match mind {
                        Some(mind) if self.kin.children.contains(&mind) => {
                            if !named.contains(&mind) {
                                named.push(mind);
                            }
                        }
                        _ => {
                            return Err(format!(
                                "a mind is waited for only by its parent. {}",
                                self.kin_line()
                            ));
                        }
                    }
                }
                named
            }
            _ => return Err("minds is a list of your children's names".into()),
        };
        if minds.is_empty() {
            return Err(format!(
                "say which children to wait for. {}",
                self.kin_line()
            ));
        }
        Ok(minds)
    }

    /// Whether `mind` could be a child of this mind's: one under its root
    /// and not itself. A clone for any other is not this mind's to count.
    fn is_own_child(&self, mind: &Name) -> bool {
        *mind != self.name && mind.root() == self.name.root()
    }

    fn is_kin(&self, mind: &Name) -> bool {
        self.kin.parent.as_ref() == Some(mind) || self.kin.children.contains(mind)
    }

    /// The minds this one can talk to, as a tool use that named a wrong one
    /// is told them.
    fn kin_line(&self) -> String {
        let parent = self.kin.parent.as_ref().map_or_else(
            || "You have no parent.".to_owned(),
            |parent| format!("Your parent is {parent}."),
        );
        if self.kin.children.is_empty() {
            format!("{parent} You have no children.")
        } else {
            format!("{parent} Your children are {}.", join(&self.kin.children))
        }
    }

    /// The hand a hand tool's use runs on. A use that names none means the
    /// only hand the mind had when the step was stored.
    fn hand_for(
        &self,
        named: Option<Result<Name, String>>,
        step_at: usize,
        holders: &[Name],
        tool: &str,
    ) -> Result<Name, String> {
        let hand = match named {
            Some(Ok(hand)) => hand,
            Some(Err(bad)) => {
                return Err(format!(
                    "{bad} is not the name of a hand. These hands can do {tool}: {}.",
                    join(holders)
                ));
            }
            None => {
                let mut then = self.hands.keys().filter(|hand| self.had(hand, step_at));
                match (then.next(), then.next()) {
                    (Some(only), None) => only.clone(),
                    (None, _) => return Err("there was no hand to run this".into()),
                    (Some(_), Some(_)) => {
                        return Err(format!(
                            "say which hand runs this. These hands can do {tool}: {}.",
                            join(holders)
                        ));
                    }
                }
            }
        };
        if !self.is_known(&hand) {
            return Err(format!(
                "there is no hand named {hand}. These hands can do {tool}: {}.",
                join(holders)
            ));
        }
        if !holders.contains(&hand) {
            return Err(format!(
                "{hand} cannot do {tool}. These hands can: {}.",
                join(holders)
            ));
        }
        Ok(hand)
    }

    /// The tool a use named `name` means. A name that hands coming or going
    /// have made mean more than one tool means the one of the hand the use
    /// names, or of the one hand the mind had when the step was stored.
    fn meant<'t>(
        &self,
        table: &'t Table,
        name: &str,
        named: Option<&Result<Name, String>>,
        step_at: usize,
    ) -> Option<(&'t Does, &'t [Name])> {
        let found = table.find_all(name);
        if let [only] = found.as_slice() {
            return Some(*only);
        }
        let hand = if let Some(Ok(hand)) = named {
            hand.clone()
        } else {
            let mut then = self.hands.keys().filter(|hand| self.had(hand, step_at));
            then.next().filter(|_| then.next().is_none()).cloned()?
        };
        let mut theirs = found
            .into_iter()
            .filter(|(_, holders)| holders.contains(&hand));
        theirs.next().filter(|_| theirs.next().is_none())
    }

    /// Whether the mind had `hand`, not released, when the step at `step_at`
    /// was stored.
    fn had(&self, hand: &Name, step_at: usize) -> bool {
        let step = self.before + index(step_at);
        self.hands.get(hand).is_some_and(|hand| {
            hand.joined < step && hand.released.is_none_or(|released| released > step)
        })
    }

    /// The hands, as a tool use that named a wrong one is told them.
    fn hands_line(&self) -> String {
        let hands: Vec<Name> = self
            .hands
            .iter()
            .filter(|(_, hand)| hand.released.is_none())
            .map(|(name, _)| name.clone())
            .collect();
        if hands.is_empty() {
            "You have no hands.".into()
        } else {
            format!("Your hands are: {}.", join(&hands))
        }
    }

    /// A name for a clone of `hand`: `<hand>-<n>`, the lowest `n` from 2 no
    /// hand has had.
    fn clone_name(&self, hand: &Name) -> Option<Name> {
        // Of one more name than there are hands, one is free.
        (2..=self.hands.len() + 2)
            .map(|n| Name::new(format!("{hand}-{n}")))
            .find(|name| {
                name.as_ref()
                    .map_or(true, |name| !self.hands.contains_key(name))
            })?
            .ok()
    }

    /// Whether the mind may ask its model: while a hand is connected, or
    /// once it has let every hand it had go. A mind that has never had a
    /// hand waits for its first, and one that has ended asks nothing.
    fn may_ask(&self) -> bool {
        !self.ended
            && (self.hands.values().any(|hand| hand.attached)
                || (!self.hands.is_empty() && self.unreleased() == 0))
    }

    fn is_known(&self, hand: &Name) -> bool {
        self.hands
            .get(hand)
            .is_some_and(|hand| hand.released.is_none())
    }

    fn is_attached(&self, hand: &Name) -> bool {
        self.hands.get(hand).is_some_and(|hand| hand.attached)
    }

    fn unreleased(&self) -> usize {
        self.hands
            .values()
            .filter(|hand| hand.released.is_none())
            .count()
    }

    /// The clones it made with `clone_hand` and has not released.
    fn clones(&self) -> usize {
        self.hands
            .values()
            .filter(|hand| hand.record.cloned && hand.released.is_none())
            .count()
    }

    fn table(&self) -> Table {
        Table::new(
            self.hands
                .iter()
                .filter(|(_, hand)| hand.released.is_none())
                .map(|(name, hand)| (name, hand.record.capabilities.as_slice())),
            self.kin.parent.is_some() || !self.kin.children.is_empty(),
            &self.settings.tools,
        )
    }

    /// The next request, and its fingerprint: for a step, or for a summary
    /// when the conversation has outgrown what one request may hold. The
    /// fingerprint is taken from the request as it is sent, and decides
    /// whose reasoning its messages carry.
    fn request(&self) -> (ModelRequest, String) {
        let mut request = ModelRequest {
            seen: self.length(),
            model: self.settings.own.model.clone(),
            effort: self.settings.own.effort.clone(),
            system: self.settings.own.system.clone(),
            messages: Vec::new(),
            tools: self.table().tools(),
            text_only: false,
        };
        let under = request.fingerprint();
        let max = self.settings.max_results;
        request.messages = window(&self.items, self.before, max, &under);
        if self.is_full(&request.messages) {
            request.messages = to_summarise(&self.items, self.before, SUMMARISE, max, &under);
            request.text_only = true;
        }
        (request, under)
    }

    /// Whether the request `messages` make would hold `compact_at` tokens
    /// or more: those the last step read and wrote, and the results that
    /// have followed it, which no model has counted yet. Nothing is full
    /// that a summary has just replaced.
    ///
    /// Not every server says what a step read. The mind then counts the
    /// whole request itself, as it counts results: a token for every two
    /// bytes.
    fn is_full(&self, messages: &[Message]) -> bool {
        let Some(limit) = self.settings.compact_at else {
            return false;
        };
        let round = self.round();
        let Some(Item::Step { usage, .. }) = round.first() else {
            return false;
        };
        if round
            .iter()
            .any(|item| matches!(item, Item::Summary { .. }))
        {
            return false;
        }
        let results = |content: &Content| match content {
            Content::ToolResult { text, .. } => text.len(),
            Content::Text(_) | Content::ToolUse { .. } | Content::Other(_) => 0,
        };
        let everything = |content: &Content| match content {
            Content::Text(text) | Content::ToolResult { text, .. } => text.len(),
            Content::ToolUse { input, .. } => input.to_string().len(),
            Content::Other(value) => value.to_string().len(),
        };
        let counted = if usage.input == 0 {
            let said = messages.iter().flat_map(|message| &message.content);
            index(said.map(everything).sum::<usize>() / 2)
        } else {
            let last = messages.last().into_iter();
            let added: usize = last.flat_map(|message| &message.content).map(results).sum();
            usage.input + usage.output + index(added / 2)
        };
        counted >= limit
    }

    /// The round in progress: the last step and everything after it. A tool
    /// use is answered within its own round, so nothing before it is read
    /// to find a call or an outcome, however long the history.
    fn round(&self) -> &[Item] {
        let from = self
            .items
            .iter()
            .rposition(|item| matches!(item, Item::Step { .. }));
        &self.items[from.unwrap_or(0)..]
    }

    /// The last step, where it is among the items, and what it said.
    fn last_step(&self) -> Option<(usize, &[Block])> {
        self.items
            .iter()
            .enumerate()
            .rev()
            .find_map(|(at, item)| match item {
                Item::Step { content, .. } => Some((at, content.as_slice())),
                _ => None,
            })
    }

    fn outcome_of(&self, tool_use: &str) -> Option<&Outcome> {
        self.round().iter().find_map(|item| match item {
            Item::Outcome {
                tool_use: of,
                outcome,
                ..
            } if of == tool_use => Some(outcome),
            _ => None,
        })
    }

    /// The hand a tool use's call went to, while the call has no outcome.
    fn called(&self, tool_use: &str) -> Option<Name> {
        self.round().iter().find_map(|item| match item {
            Item::Call {
                hand, tool_use: of, ..
            } if of == tool_use => Some(hand.clone()),
            _ => None,
        })
    }

    fn call_of(&self, tool_use: &str) -> Option<CallId> {
        self.round().iter().find_map(|item| match item {
            Item::Call {
                id, tool_use: of, ..
            } if of == tool_use => Some(*id),
            _ => None,
        })
    }

    fn call(&self, id: CallId) -> Option<Call> {
        self.round().iter().find_map(|item| match item {
            Item::Call {
                id: of,
                capability,
                operation,
                input,
                ..
            } if *of == id => Some(Call {
                id,
                capability: capability.clone(),
                operation: operation.clone(),
                input: input.clone(),
            }),
            _ => None,
        })
    }

    /// The calls with no outcome yet, oldest first, each with its hand and
    /// its tool use.
    fn pending(&self) -> Vec<(CallId, Name, String)> {
        self.round()
            .iter()
            .filter_map(|item| match item {
                Item::Call {
                    id, hand, tool_use, ..
                } if self.outcome_of(tool_use).is_none() => {
                    Some((*id, hand.clone(), tool_use.clone()))
                }
                _ => None,
            })
            .collect()
    }

    /// The last step's tool uses that have neither a call nor an outcome.
    fn unissued(&self) -> Vec<String> {
        self.last_step()
            .into_iter()
            .flat_map(|(_, content)| content)
            .filter_map(|block| match block {
                Block::ToolUse { id, .. }
                    if self.outcome_of(id).is_none() && self.call_of(id).is_none() =>
                {
                    Some(id.clone())
                }
                _ => None,
            })
            .collect()
    }

    /// The id of the last call made, or 0: ids only grow, so the last is
    /// the highest. A summary says what it was when it was written, and a
    /// mind made from another's history what the other's was then.
    fn last_call(&self) -> CallId {
        let last = self.items.iter().rev().find_map(|item| match item {
            Item::Call { id, .. } => Some(*id),
            Item::Summary { last_call, .. } | Item::Branched { last_call, .. } => Some(*last_call),
            _ => None,
        });
        last.unwrap_or(0)
    }
}

/// What an item at index `at` changes in what a mind knows of its hands,
/// its kin, and whether it has ended.
fn note(hands: &mut BTreeMap<Name, Hand>, kin: &mut Kin, ended: &mut bool, item: &Item, at: u64) {
    match item {
        // What a summary carries is what the mind knew when it was written.
        Item::Summary {
            hands: recorded,
            forks_left,
            parent,
            children,
            ..
        } => {
            for (name, record) in recorded {
                hands.entry(name.clone()).or_insert_with(|| Hand {
                    record: record.clone(),
                    attached: false,
                    joined: 0,
                    released: record.released.then_some(0),
                });
            }
            *kin = Kin {
                parent: parent.clone(),
                children: children.clone(),
                forks_left: *forks_left,
            };
        }
        Item::Attached {
            hand,
            boot,
            capabilities,
        } => {
            let known = hands.entry(hand.clone()).or_insert_with(|| Hand {
                record: HandRecord {
                    boot: String::new(),
                    capabilities: Vec::new(),
                    released: false,
                    cloned: false,
                },
                attached: false,
                joined: at,
                released: None,
            });
            known.record.boot.clone_from(boot);
            known.record.capabilities.clone_from(capabilities);
        }
        Item::Cloned { hand, from } => {
            let capabilities = hands
                .get(from)
                .map(|from| from.record.capabilities.clone())
                .unwrap_or_default();
            let known = hands.entry(hand.clone()).or_insert_with(|| Hand {
                // No process of it has said hello: its first will be new.
                record: HandRecord {
                    boot: String::new(),
                    capabilities,
                    released: false,
                    cloned: true,
                },
                attached: false,
                joined: at,
                released: None,
            });
            // It may have said hello before its original's result was stored.
            known.record.cloned = true;
        }
        Item::Released { hand, .. } => {
            if let Some(known) = hands.get_mut(hand) {
                known.record.released = true;
                known.attached = false;
                known.released = Some(at);
            }
        }
        Item::Forked { children, left } => {
            for child in children {
                if !kin.children.contains(child) {
                    kin.children.push(child.clone());
                }
            }
            kin.forks_left = *left;
        }
        // A child's history holds its parent's: the parent's children are
        // its siblings.
        Item::Parted { parent, left } => {
            // The hand it was forked onto is its own, not a clone it made.
            for known in hands.values_mut() {
                known.record.cloned = false;
            }
            *kin = Kin {
                parent: Some(parent.clone()),
                children: Vec::new(),
                forks_left: *left,
            };
        }
        // A mind made from another's history is no kin of it, and the hand
        // it keeps is its own.
        Item::Branched { left, .. } => {
            for known in hands.values_mut() {
                known.record.cloned = false;
            }
            *kin = Kin {
                parent: None,
                children: Vec::new(),
                forks_left: *left,
            };
        }
        Item::Ended => *ended = true,
        _ => {}
    }
}

/// Why a use named `name` means no tool: no tool has the name, or hands
/// coming and going have made it mean several, which have names of their
/// own.
fn not_meant(table: &Table, name: &str) -> String {
    let tools: Vec<String> = table
        .named(name)
        .into_iter()
        .map(|(tool, holders)| format!("{tool} ({})", join(holders)))
        .collect();
    if tools.is_empty() {
        format!("there is no tool named {name}")
    } else {
        format!(
            "{name} now means more than one tool: {}. Ask for one of them by its name.",
            tools.join(", ")
        )
    }
}

fn join(names: &[Name]) -> String {
    let names: Vec<&str> = names.iter().map(Name::as_str).collect();
    names.join(", ")
}

fn index(len: usize) -> u64 {
    u64::try_from(len).expect("a history is shorter than u64::MAX items")
}

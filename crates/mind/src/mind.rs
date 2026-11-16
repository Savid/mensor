use mensor_protocol::{Call, CallId, CallResult, FromMind, Hello, Settled, settle};
use serde_json::Value;

use crate::window::{to_summarise, window};
use crate::{Block, Content, Item, Message, ModelReply, ModelRequest, Outcome, Stop, Tool, tools};

/// Something the host of a mind must do. A host performs a mind's outputs in
/// the order given, and makes each [`Output::Append`] durable before it
/// performs what follows it.
#[derive(Clone, Debug, PartialEq)]
pub enum Output {
    /// Store this item at the end of the history.
    Append(Item),
    /// Send this to the hand.
    Send(FromMind),
    /// Ask the model, and answer with [`Mind::replied`] or [`Mind::failed`].
    Ask(ModelRequest),
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

/// What a mind is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Nothing, until it is prompted.
    Idle,
    /// It has a model to ask, or is asking it.
    Thinking,
    /// It has a call for its hand, or waits on one.
    Working,
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

/// A prompt whose key the history already holds with other text.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("another prompt already has the key {key:?}")]
pub struct Conflict {
    /// The key both prompts carry.
    pub key: String,
}

/// What a mind is told when it is made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// The system prompt the mind gives its model.
    pub system: Option<String>,
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
}

impl Default for Settings {
    /// No system prompt, a summary once a request would pass 150,000
    /// tokens, and 96 KB of results a step, which is 40,000 tokens at the
    /// most. The summary is asked for with the last step's results, so a
    /// model that reads 200,000 has room for it.
    fn default() -> Self {
        Self {
            system: None,
            compact_at: Some(150_000),
            max_results: 96 * 1024,
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
    /// How many items of the history come before `items`.
    before: u64,
    items: Vec<Item>,
    settings: Settings,
    tools: Vec<Tool>,
    attached: bool,
    /// The request the model is answering now.
    asking: Option<Asking>,
}

#[derive(Clone, Copy, Debug)]
struct Asking {
    seen: u64,
    /// Whether the model was asked for a summary, not a step.
    summary: bool,
}

enum Next {
    /// A tool use in a reply that did not stop for tools: it does not run.
    Skip {
        tool_use: String,
    },
    Issue {
        tool_use: String,
        name: String,
        input: Value,
    },
    Await,
    Ask,
    Idle,
}

impl Mind {
    /// A mind with no history.
    pub fn new(settings: Settings) -> Self {
        Self::load(0, Vec::new(), settings)
    }

    /// A mind with this history, as stored: its items from index `from`,
    /// which is 0 for all of it, or else the index a store gives with a
    /// history's tail. It does nothing until its hand says hello.
    pub fn load(from: u64, items: Vec<Item>, settings: Settings) -> Self {
        let mut mind = Self {
            before: from,
            items,
            settings,
            tools: Vec::new(),
            attached: false,
            asking: None,
        };
        mind.shed();
        mind
    }

    /// What the mind of a history is doing, and how many items the history
    /// holds, told from its latest items alone: those from index `from`,
    /// which a store gives as a history's [`recent`](crate::Store::recent)
    /// items. A host says this of a pair it has not loaded.
    pub fn status(from: u64, items: Vec<Item>) -> (State, u64) {
        let mind = Self {
            before: from,
            ..Self::whole(items, Settings::default())
        };
        (mind.state(), mind.length())
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

    /// Whether a hand is connected and settled.
    pub fn is_attached(&self) -> bool {
        self.attached
    }

    /// What the mind is doing.
    pub fn state(&self) -> State {
        match self.next() {
            Next::Idle => State::Idle,
            Next::Ask => State::Thinking,
            Next::Skip { .. } | Next::Issue { .. } | Next::Await => State::Working,
        }
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
    pub fn prompt(
        &mut self,
        key: Option<String>,
        text: String,
        earlier: Option<&str>,
    ) -> Result<Prompted, Conflict> {
        if let Some(key) = &key {
            let held = self.items.iter().find_map(|item| match item {
                Item::Prompt {
                    key: Some(held),
                    text,
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
                Some(_) => return Err(Conflict { key: key.clone() }),
                None => {}
            }
        }
        let mut outputs = Vec::new();
        self.append(Item::Prompt { key, text }, &mut outputs);
        self.drive(&mut outputs);
        Ok(Prompted {
            existing: false,
            outputs,
        })
    }

    /// Takes a hand's `hello` and settles every call the mind waits on.
    ///
    /// Results the hand holds are stored, calls it may have run without a
    /// result are stored as unknown, and only then is anything sent: acks for
    /// what the hand holds, and again each call it never read.
    ///
    /// A turn interrupted while the hand was away is stopped now: a call the
    /// hand never read does not run, and one it still runs is cancelled.
    pub fn attach(&mut self, hello: &Hello) -> Vec<Output> {
        let pending = self.pending();
        let ids: Vec<CallId> = pending.iter().map(|(id, _)| *id).collect();
        let plan = settle(self.recorded_boot(), &ids, hello);
        let interrupted = self
            .round()
            .iter()
            .any(|item| matches!(item, Item::Paused { reason } if reason == INTERRUPTED));

        let mut outputs = Vec::new();
        let mut resend = Vec::new();
        let mut cancel = Vec::new();
        for ((id, tool_use), (_, settled)) in pending.into_iter().zip(plan.calls) {
            let outcome = match settled {
                Settled::Take(reply) => Outcome::from(reply),
                Settled::Unknown if plan.new_boot => Outcome::Unknown(
                    "the hand restarted after it was sent this call, and remembers nothing of it"
                        .into(),
                ),
                Settled::Unknown => {
                    Outcome::Unknown("the hand read this call but holds no result for it".into())
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
            self.append(
                Item::Outcome {
                    tool_use,
                    call: Some(id),
                    outcome,
                },
                &mut outputs,
            );
        }
        if plan.new_boot {
            self.append(
                Item::Attached {
                    boot: hello.boot.clone(),
                },
                &mut outputs,
            );
        }

        self.tools = tools(&hello.capabilities);
        self.attached = true;
        if !plan.ack.is_empty() {
            outputs.push(Output::Send(FromMind::Ack { ids: plan.ack }));
        }
        for id in resend {
            if let Some(call) = self.call(id) {
                outputs.push(Output::Send(FromMind::Call(call)));
            }
        }
        for id in cancel {
            outputs.push(Output::Send(FromMind::Cancel { id }));
        }
        self.drive(&mut outputs);
        outputs
    }

    /// The hand's connection dropped. The mind starts nothing until the hand
    /// says hello again.
    pub fn detach(&mut self) {
        self.attached = false;
    }

    /// Takes a result from the hand. The first result for a call is stored
    /// and acked; any other is only acked.
    pub fn result(&mut self, result: CallResult) -> Vec<Output> {
        let mut outputs = Vec::new();
        let waiting = self.pending().into_iter().find(|(id, _)| *id == result.id);
        if let Some((id, tool_use)) = waiting {
            let outcome = Outcome::from(result.reply);
            self.append(
                Item::Outcome {
                    tool_use,
                    call: Some(id),
                    outcome,
                },
                &mut outputs,
            );
        }
        outputs.push(Output::Send(FromMind::Ack {
            ids: vec![result.id],
        }));
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
                // A mind carries on from the last step's prompts, the hand
                // the history last recorded and the last call it made.
                let from = match self.round().first() {
                    Some(Item::Step { seen, .. }) => *seen,
                    _ => self.length(),
                };
                Item::Summary {
                    text,
                    usage,
                    from,
                    boot: self.recorded_boot().map(str::to_owned),
                    last_call: self.last_call(),
                }
            }
        } else {
            Item::Step {
                seen,
                content,
                stop,
                usage,
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
    /// uses that had not become calls do not run, and the hand is told to
    /// cancel every call it runs. The mind is then paused: it stores what
    /// the hand returns for those calls, and asks no model until it is
    /// prompted. A hand that is away is told when it next says hello. A mind
    /// with nothing to stop does nothing.
    pub fn interrupt(&mut self) -> Vec<Output> {
        let mut outputs = Vec::new();
        if self.asking.take().is_none() && matches!(self.next(), Next::Idle) {
            return outputs;
        }
        for tool_use in self.unissued() {
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
        for (id, _) in self.pending() {
            outputs.push(Output::Send(FromMind::Cancel { id }));
        }
        outputs
    }

    /// A mind that holds these items, with nothing shed.
    fn whole(items: Vec<Item>, settings: Settings) -> Self {
        Self {
            before: 0,
            items,
            settings,
            tools: Vec::new(),
            attached: false,
            asking: None,
        }
    }

    /// Forgets what the last summary replaced: every item before the index
    /// it names.
    pub(crate) fn shed(&mut self) {
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

    pub(crate) fn append(&mut self, item: Item, outputs: &mut Vec<Output>) {
        self.items.push(item.clone());
        outputs.push(Output::Append(item));
    }

    /// Does everything the history allows, up to the next thing to wait for.
    fn drive(&mut self, outputs: &mut Vec<Output>) {
        loop {
            match self.next() {
                Next::Skip { tool_use } => {
                    let outcome = Outcome::NotRun(
                        "the model stopped before it finished asking for this call".into(),
                    );
                    self.append(
                        Item::Outcome {
                            tool_use,
                            call: None,
                            outcome,
                        },
                        outputs,
                    );
                }
                Next::Issue {
                    tool_use,
                    name,
                    input,
                } if self.attached => {
                    let Some(tool) = self.tools.iter().find(|tool| tool.name == name) else {
                        let outcome = Outcome::NotRun(format!("the hand has no tool named {name}"));
                        self.append(
                            Item::Outcome {
                                tool_use,
                                call: None,
                                outcome,
                            },
                            outputs,
                        );
                        continue;
                    };
                    let call = Call {
                        id: self.last_call() + 1,
                        capability: tool.capability.clone(),
                        operation: tool.operation.clone(),
                        input,
                    };
                    let stored = Item::Call {
                        id: call.id,
                        tool_use,
                        capability: call.capability.clone(),
                        operation: call.operation.clone(),
                        input: call.input.clone(),
                    };
                    self.append(stored, outputs);
                    outputs.push(Output::Send(FromMind::Call(call)));
                }
                Next::Ask if self.attached && self.asking.is_none() => {
                    let (request, summary) = self.request();
                    self.asking = Some(Asking {
                        seen: request.seen,
                        summary,
                    });
                    outputs.push(Output::Ask(request));
                    return;
                }
                Next::Issue { .. } | Next::Await | Next::Ask | Next::Idle => return,
            }
        }
    }

    fn next(&self) -> Next {
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
        if let Some((_, _, content, stop)) = step {
            for block in content {
                let Block::ToolUse { id, name, input } = block else {
                    continue;
                };
                if self.outcome_of(id).is_some() {
                    continue;
                }
                if self.call_of(id).is_some() {
                    return Next::Await;
                }
                if stop != Stop::ToolUse {
                    return Next::Skip {
                        tool_use: id.clone(),
                    };
                }
                return Next::Issue {
                    tool_use: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                };
            }
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

    /// The next request: for a step, or for a summary when the conversation
    /// has outgrown what one request may hold.
    fn request(&self) -> (ModelRequest, bool) {
        let max = self.settings.max_results;
        let mut messages = window(&self.items, self.before, max);
        let summary = self.is_full(&messages);
        if summary {
            messages = to_summarise(&self.items, self.before, SUMMARISE, max);
        }
        let request = ModelRequest {
            seen: self.length(),
            system: self.settings.system.clone(),
            messages,
            tools: self.tools.clone(),
            text_only: summary,
        };
        (request, summary)
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

    fn call_of(&self, tool_use: &str) -> Option<CallId> {
        self.round().iter().find_map(|item| match item {
            Item::Call {
                id, tool_use: of, ..
            } if of == tool_use => Some(*id),
            _ => None,
        })
    }

    fn call(&self, id: CallId) -> Option<Call> {
        self.items.iter().rev().find_map(|item| match item {
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

    /// The calls with no outcome yet, oldest first, each with its tool use.
    pub(crate) fn pending(&self) -> Vec<(CallId, String)> {
        self.round()
            .iter()
            .filter_map(|item| match item {
                Item::Call { id, tool_use, .. } if self.outcome_of(tool_use).is_none() => {
                    Some((*id, tool_use.clone()))
                }
                _ => None,
            })
            .collect()
    }

    /// The last step's tool uses that have neither a call nor an outcome.
    pub(crate) fn unissued(&self) -> Vec<String> {
        let content = self.items.iter().rev().find_map(|item| match item {
            Item::Step { content, .. } => Some(content),
            _ => None,
        });
        content
            .into_iter()
            .flatten()
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

    /// The hand process the history last recorded. A summary says which
    /// it was when it was written, for a mind that holds nothing older.
    pub(crate) fn recorded_boot(&self) -> Option<&str> {
        let last = self.items.iter().rev().find_map(|item| match item {
            Item::Attached { boot } => Some(Some(boot.as_str())),
            Item::Summary { boot, .. } => Some(boot.as_deref()),
            _ => None,
        });
        last.flatten()
    }

    /// The id of the last call made, or 0: ids only grow, so the last is
    /// the highest. A summary says what it was when it was written.
    fn last_call(&self) -> CallId {
        let last = self.items.iter().rev().find_map(|item| match item {
            Item::Call { id, .. } => Some(*id),
            Item::Summary { last_call, .. } => Some(*last_call),
            _ => None,
        });
        last.unwrap_or(0)
    }
}

fn index(len: usize) -> u64 {
    u64::try_from(len).expect("a history is shorter than u64::MAX items")
}

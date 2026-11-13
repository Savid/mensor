use mensor_protocol::{CallId, Hello};

use crate::{Item, Mind, Outcome, Output, Settings};

/// A mind made for a cloned body.
#[derive(Clone, Debug)]
pub struct Fork {
    /// How many of the parent's items the new mind shares. Its own items
    /// follow from this index.
    pub inherited: u64,
    /// The new mind, already attached to its body.
    pub mind: Mind,
    /// What the new mind's host must do, starting with the first items of
    /// its own.
    pub outputs: Vec<Output>,
}

/// Why a mind could not fork.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ForkError {
    /// The clone has read a call its parent's mind never made, so it is not
    /// a clone of that mind's body.
    #[error("the clone has read call {0}, which this mind never made")]
    UnknownCall(CallId),
}

/// Where a clone's history parts from its parent's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Parting {
    /// How many of the parent's items the clone shares.
    pub cut: u64,
    /// The index the clone's mind starts from: the one the last summary
    /// they share names, or 0. The clone's mind is made from the parent's
    /// items from here up to `cut`.
    pub from: u64,
}

/// Finds where a clone's history parts from its parent's.
///
/// A host reads the parent's history to it once, in order, a page at a
/// time, so that a fork holds no more of a long history than one page.
///
/// A clone whose boot the history records kept its memory, and says how far
/// it had read. Everything up to that call is shared. What follows the call
/// is shared too, up to the next thing that touched the body (the next
/// call, or a new boot), unless the clone still runs the call or holds its
/// result: then its own outcome may differ from the one the history holds,
/// and nothing after the call is shared.
///
/// A clone whose boot is new remembers nothing, and shares everything.
#[derive(Clone, Debug)]
pub struct Cut {
    boot: String,
    last_id: CallId,
    /// Whether the clone still runs the last call it read, or holds its
    /// result.
    with_clone: bool,
    /// How many items have been read.
    at: u64,
    /// Where the history first records the clone's boot.
    booted: Option<u64>,
    /// Where the clone had read to: the last call it read, or its boot if
    /// it had read none.
    read: Option<u64>,
    /// The next thing after that to touch the body.
    next: Option<u64>,
    /// Each summary read: its index, and the index it names.
    summaries: Vec<(u64, u64)>,
}

impl Cut {
    /// A search for where the body that says `hello` parted from the one
    /// it is a clone of.
    pub fn new(hello: &Hello) -> Self {
        let last = hello.last_id;
        Self {
            boot: hello.boot.clone(),
            last_id: last,
            with_clone: hello.running.contains(&last)
                || hello.held.iter().any(|held| held.id == last),
            at: 0,
            booted: None,
            read: None,
            next: None,
            summaries: Vec::new(),
        }
    }

    /// Takes the next items of the parent's history.
    pub fn read(&mut self, items: &[Item]) {
        for item in items {
            let at = self.at;
            self.at += 1;
            match item {
                Item::Attached { boot } if *boot == self.boot && self.booted.is_none() => {
                    self.booted = Some(at);
                    if self.last_id == 0 {
                        self.read = Some(at);
                        continue;
                    }
                }
                Item::Call { id, .. } if *id == self.last_id && self.read.is_none() => {
                    self.read = Some(at);
                    continue;
                }
                Item::Summary { from, .. } => self.summaries.push((at, *from)),
                _ => {}
            }
            let touches = matches!(item, Item::Call { .. } | Item::Attached { .. });
            if touches && self.next.is_none() && self.read.is_some() {
                self.next = Some(at);
            }
        }
    }

    /// Where the two part, once the whole history has been read. An error
    /// when the clone has read a call the history does not hold.
    pub fn end(self) -> Result<Parting, ForkError> {
        let cut = if self.booted.is_none() {
            self.at
        } else {
            let read = self.read.ok_or(ForkError::UnknownCall(self.last_id))?;
            if self.with_clone {
                read + 1
            } else {
                self.next.unwrap_or(self.at)
            }
        };
        let shared = self.summaries.iter().rev().find(|(at, _)| *at < cut);
        Ok(Parting {
            cut,
            from: shared.map_or(0, |(_, from)| *from),
        })
    }
}

/// The mind for a clone of a body, in the making.
///
/// It is made from the end of the history the clone shares. If the clone
/// has lost calls its parent's body went on to finish, a host then reads it
/// the parent's later items, a page at a time, for how those calls ended.
///
/// Tool uses that had not yet become calls do not run. The new mind is then
/// paused: it settles what its body still holds or runs, and calls no model
/// until it is prompted.
#[derive(Clone, Debug)]
pub struct Forking {
    mind: Mind,
    inherited: u64,
    hello: Hello,
    /// Whether the clone kept its memory: the history's last boot is its.
    remembers: bool,
    /// The calls the clone no longer has, each with its tool use, and how
    /// the parent's history says it ended, once that is read.
    lost: Vec<(CallId, String, Option<Outcome>)>,
}

impl Forking {
    /// Starts the mind of the clone that says `hello`, from `shared`: its
    /// parent's items from `parting.from` up to `parting.cut`.
    pub fn new(parting: Parting, shared: Vec<Item>, settings: Settings, hello: &Hello) -> Self {
        let mind = Mind::load(parting.from, shared, settings);
        let remembers = mind.recorded_boot() == Some(hello.boot.as_str());
        let with_clone = |id: &CallId| {
            hello.running.contains(id) || hello.held.iter().any(|held| held.id == *id)
        };
        let lost = if remembers {
            let pending = mind.pending().into_iter();
            let lost = pending.filter(|(id, _)| !with_clone(id));
            lost.map(|(id, tool_use)| (id, tool_use, None)).collect()
        } else {
            Vec::new()
        };
        Self {
            mind,
            inherited: parting.cut,
            hello: hello.clone(),
            remembers,
            lost,
        }
    }

    /// Whether the parent's items from `parting.cut` on are still wanted:
    /// some call the clone has lost has no outcome yet.
    pub fn wants(&self) -> bool {
        self.lost.iter().any(|(_, _, outcome)| outcome.is_none())
    }

    /// Takes the next of the parent's later items.
    pub fn later(&mut self, items: &[Item]) {
        for item in items {
            let Item::Outcome {
                call: Some(call),
                outcome,
                ..
            } = item
            else {
                continue;
            };
            let mut lost = self.lost.iter_mut();
            if let Some((_, _, ended)) = lost.find(|(id, _, ended)| id == call && ended.is_none()) {
                *ended = Some(outcome.clone());
            }
        }
    }

    /// The new mind, attached to its body, and what its host must do.
    pub fn end(self) -> Fork {
        let mut mind = self.mind;
        let mut outputs = Vec::new();
        for (id, tool_use, outcome) in self.lost {
            if let Some(outcome) = outcome {
                let call = Some(id);
                let item = Item::Outcome {
                    tool_use,
                    call,
                    outcome,
                };
                mind.append(item, &mut outputs);
            }
        }
        for tool_use in mind.unissued() {
            let outcome = Outcome::NotRun("the mind was forked before this call started".into());
            let item = Item::Outcome {
                tool_use,
                call: None,
                outcome,
            };
            mind.append(item, &mut outputs);
        }
        let mut reason = match &self.hello.was {
            Some(parent) => format!("forked from {parent}"),
            None => "forked".into(),
        };
        if !self.remembers {
            reason.push_str(", as a copy of its body made without its memory");
        }
        mind.append(Item::Paused { reason }, &mut outputs);
        outputs.extend(mind.attach(&self.hello));
        mind.shed();
        Fork {
            inherited: self.inherited,
            mind,
            outputs,
        }
    }
}

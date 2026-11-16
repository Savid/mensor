//! The mind core: a journal, a turn loop and a fork, with no I/O.
//!
//! A mind is a history and a loop: ask the model, do what it asked for through
//! the hand, ask again. A [`Mind`] is that loop as a state machine. A host
//! feeds it what happens (a prompt, a hand's `hello`, a result, a model's
//! reply) and it answers with [`Output`]s for the host to perform in order:
//! store an [`Item`], send a message to the hand, ask the model.
//!
//! The history is the whole state. A mind rebuilt from its stored items with
//! [`Mind::load`] carries on where the old one stopped, and a mind made by
//! [`Forking`] shares its parent's history up to the point its cloned hand
//! had reached.
//!
//! A mind holds only the end of a long history. Once its model has summed a
//! conversation up, the mind forgets the items that summary replaced, and a
//! mind loaded later is given only a [`Store`]'s tail of the history. The
//! store keeps every item.
//!
//! Four rules hold throughout:
//!
//! - **Store before send.** A call is an item in the history before it is
//!   sent to the hand, so the history always knows what the hand may have done.
//! - **No call runs twice.** A call the hand may have run is never sent again:
//!   its outcome is recorded as unknown and the model is told to look.
//! - **One mind, one hand.** A clone of the hand gets a mind of its own.
//! - **New minds start idle.** A forked mind calls no model until prompted.
//!
//! This crate is pure: no I/O, no clock and no randomness.

mod fork;
mod item;
mod mind;
mod model;
mod store;
mod window;

pub use fork::{Cut, Fork, ForkError, Forking, Parting};
pub use item::{Block, Item, Outcome, Stop, Usage};
pub use mind::{Conflict, Mind, Output, Prompted, Settings, State};
pub use model::{Content, Message, ModelError, ModelReply, ModelRequest, Role, Tool, tools};
pub use store::{MemoryStore, Origin, Pair, Store, StoreError, prompt_under, tail_from};

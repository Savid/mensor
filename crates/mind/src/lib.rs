//! The mind core: a journal, a turn loop and a fork, with no I/O.
//!
//! A mind is a history and a loop: ask the model, do what it asked for through
//! its hands, ask again. A [`Mind`] is that loop as a state machine. A host
//! feeds it what happens (a prompt, a hand's `hello`, a result, a model's
//! reply) and it answers with [`Output`]s for the host to perform in order:
//! store an [`Item`], send a message to a hand, ask the model, make a child
//! mind, tell another mind something, or watch for its children to rest.
//!
//! The history is the whole state. A mind rebuilt from its stored items with
//! [`Mind::load`] carries on where the old one stopped. A mind made by a
//! [`Fork`] shares its parent's history up to the call that cloned its hand.
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
//! - **One mind drives its hands.** A hand belongs to one mind; a mind has
//!   several. Calls to different hands run together, and one hand's calls
//!   keep their order.
//! - **The mind decides forks.** Its model forks by a tool of the mind's own,
//!   which asks a hand to clone itself, and the history records where the
//!   child parts from its parent. Parent and children talk through tools of
//!   their own: one tells the other, and a parent waits for its children.
//!
//! This crate is pure: no I/O, no clock and no randomness.

mod fork;
mod item;
mod mind;
mod model;
mod store;
#[cfg(any(test, feature = "suite"))]
pub mod suite;
mod tools;
mod window;

pub use fork::Fork;
pub use item::{Block, HandRecord, Item, Outcome, Stop, Usage};
pub use mind::{
    Conflict, Mind, NoHand, Output, Prompted, Report, Settings, State, Status, Unbranched, Untold,
};
pub use model::{Content, Message, ModelError, ModelReply, ModelRequest, Role, Tool};
pub use store::{
    KNOWING, Latest, MadeFrom, MemoryStore, MindRecord, MindSettings, Origin, Store, StoreError,
    Trim, prompt_under, tail_from,
};
pub use tools::{CLONE, MAX_CHILDREN, MindTool, NoMindTool};

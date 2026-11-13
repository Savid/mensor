//! A mind and its body as one object.
//!
//! Some hosts keep a whole pair in one place: a serverless object that owns
//! a small database and is woken to do a piece of work. There is no socket
//! to speak the protocol over, and no second process to dial in, but the
//! rules are the ones a server and a remote body keep. An [`Object`] holds a
//! [`Mind`](mensor_mind::Mind) and a body's [`Core`](mensor_body::Core) and
//! passes the protocol's messages between them in memory.
//!
//! The host supplies three things: a [`Storage`] for the history and for
//! what the body remembers, a [`Model`], and the body's capabilities. It
//! then calls [`step`] until there is nothing left to do:
//!
//! ```
//! use std::cell::RefCell;
//!
//! use mensor_body::Capabilities;
//! use mensor_mind::{Block, ModelError, ModelReply, ModelRequest, Settings, Stop, Usage};
//! use mensor_object::{MemoryStorage, Model, Object, step};
//! use mensor_protocol::Name;
//!
//! struct Echo;
//!
//! impl Model for Echo {
//!     async fn ask(&self, _request: &ModelRequest) -> Result<ModelReply, ModelError> {
//!         let content = vec![Block::Text { text: "hello".into() }];
//!         Ok(ModelReply { content, stop: Stop::EndTurn, usage: Usage::default() })
//!     }
//! }
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let capabilities = Capabilities::new();
//! let name = Name::new("a1")?;
//! let storage = MemoryStorage::new();
//! let settings = Settings::default();
//! let object = Object::start(name, || "boot-1".into(), capabilities.declare(), settings, storage)?;
//! let object = RefCell::new(object);
//!
//! object.borrow_mut().prompt(None, "hi".into())?;
//! while step(&object, &capabilities, &Echo).await? {}
//! assert_eq!(object.borrow().length(), 3);
//! # Ok(())
//! # }
//! ```
//!
//! An object may stop at any instant and be opened again over the same
//! storage. What it was doing then is settled as a body's reconnection is:
//! a model that was being asked is asked again, and a call that was running
//! is recorded as lost, with an outcome nobody knows, and never run twice.
//!
//! This crate is pure: no I/O, no clock and no randomness. It needs no
//! runtime, and its futures need not be `Send`.

mod object;
pub mod rows;
mod storage;

pub use object::{Model, Object, PromptError, Work, step};
pub use storage::{MemoryStorage, Storage, StorageError, Stored};

//! The hand core: what a hand remembers, and the capabilities it offers.
//!
//! A hand does what one mind asks. The [`Core`] is the small part every hand
//! shares, wherever it runs: its name, the ids it has read, the calls it is
//! running and the results it holds until the mind acks them. Everything a
//! hand can actually do is a [`Capability`] added to it.
//!
//! The core is a state machine with no I/O. A host feeds it what arrives and
//! does what it answers:
//!
//! ```
//! use mensor_hand::{Core, Effect, Limits};
//! use mensor_protocol::{Call, FromMind, Name, Reply};
//! use serde_json::json;
//!
//! let mut core = Core::new(Name::new("a1").unwrap(), "boot-1", Limits::default());
//! let call = Call { id: 1, capability: "commands".into(), operation: "bash".into(), input: json!({}) };
//!
//! // The mind asks: the host starts the call.
//! assert_eq!(core.receive(FromMind::Call(call.clone())), vec![Effect::Start(call.clone())]);
//! // The same id again never starts a second time.
//! assert!(matches!(core.receive(FromMind::Call(call))[..], [Effect::Send(_)]));
//!
//! // The call ends: the core holds its result until the mind acks it.
//! let result = core.finished(1, Reply::Output(json!("done"))).unwrap();
//! assert_eq!(core.hello(Vec::new()).held, vec![result]);
//! core.receive(FromMind::Ack { ids: vec![1] });
//! assert!(core.hello(Vec::new()).held.is_empty());
//! ```
//!
//! The core is safe to copy at any instant: a copy holds the same results and
//! refuses the same ids. Whoever copies a hand feeds the copy a new name with
//! [`Core::rename`], and the copy's next [`Hello`](mensor_protocol::Hello) tells
//! the mind's host which hand it was cloned from.

mod capability;
mod core;
mod files;

pub use capability::{CallFuture, Capabilities, Capability, input};
pub use core::{Core, Effect, Limits};
pub use files::{FileSystem, Files, FsFuture};

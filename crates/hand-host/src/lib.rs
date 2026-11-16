//! A hand on a real machine.
//!
//! This crate runs a [`mensor_hand::Core`] natively. The driver reaches the
//! mind's host through a [`Dial`], says hello, starts the calls it is sent
//! and holds their results. When the connection drops it dials again, and
//! the calls it was running carry on meanwhile. [`WebSocket`] is the dial
//! for a host across a network; a host in the same process supplies its own.
//!
//! It also has the first capabilities a hand on a machine wants:
//! [`capabilities::Commands`] runs shell commands and [`capabilities::Files`]
//! reads and writes files.
//!
//! A clone of a hand is fed its new name through [`Handle::rename`], or
//! through the name source the driver asks before every dial, and its next
//! hello tells the mind's host it is a clone.

pub mod capabilities;
mod driver;
mod transport;

pub use driver::{Backoff, Config, Error, Handle, NameSource, boot, start};
pub use transport::{BoxFuture, Dial, DialError, Gone, Incoming, Link, WebSocket};

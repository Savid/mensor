//! The protocol a mind and a hand speak.
//!
//! A mind holds a history and calls a model. A hand does what the mind asks.
//! The protocol joins exactly one mind to exactly one hand. It keeps nothing
//! itself: every guarantee comes from what the two ends remember.
//!
//! - The mind stores a [`Call`] before it sends it, under an id that only
//!   grows.
//! - The hand refuses an id it has already read, so no call runs twice.
//! - The hand holds each [`CallResult`] until the mind acks it, so a dropped
//!   connection loses nothing.
//! - Every connection opens with a [`Hello`], and [`settle`] turns it into
//!   what becomes of each call the mind still waits on.
//!
//! The protocol names no transport. Each message is one JSON object, and the
//! first binding carries one message per WebSocket text frame.
//!
//! This crate is pure: no I/O, no clock and no randomness.

mod message;
mod name;
mod refusal;
mod settle;

pub use message::{
    Call, CallId, CallResult, CapabilityDecl, DecodeError, FromHand, FromMind, Hello,
    MAX_FRAME_BYTES, OperationDecl, Refused, Reply, VERSION, version_of,
};
pub use name::{InvalidName, Name};
pub use refusal::Refusal;
pub use settle::{Plan, Settled, settle};

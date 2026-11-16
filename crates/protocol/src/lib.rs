//! The protocol a mind and a hand speak.
//!
//! A mind holds a history and calls a model. A hand does what the mind asks.
//! A mind has several hands, each joined to it by a connection of its own,
//! and the protocol is what one hand and its mind say over theirs. It keeps
//! nothing itself: every guarantee comes from what the two ends remember.
//!
//! - The mind stores a [`Call`] before it sends it, under an id that only
//!   grows. One mind numbers the calls of all its hands, so each hand sees
//!   ids that rise.
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
    MAX_FRAME_BYTES, OperationDecl, Refused, Reply,
};
pub use name::{InvalidName, Name};
pub use refusal::Refusal;
pub use settle::{Plan, Settled, settle};

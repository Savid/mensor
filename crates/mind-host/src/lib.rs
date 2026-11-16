//! A server that hosts minds.
//!
//! The [`Host`] is the process around the pure [`mensor_mind::Mind`]. It keeps
//! every history in a [`Store`](mensor_mind::Store), runs one task for each
//! pair that is awake, asks a [`Model`], and serves two things over HTTP:
//!
//! - **Attach**, at `GET /v1/attach`: a WebSocket a hand dials. A name the
//!   host knows is that hand reconnecting. A new name with no `was` is a new
//!   pair. A new name with a `was` is a clone, and the mind it names forks.
//! - **The API**, under `/v1/minds`: list pairs, prompt one under a key,
//!   interrupt its turn, read its history, and follow it as server-sent
//!   events from a cursor, with the text of each reply as the model writes
//!   it.
//!
//! The host performs each of a mind's outputs in order, and an item is in the
//! store before anything it describes is sent.
//!
//! A pair is awake while its hand is connected, someone follows it or its
//! model is asked, and for [`Config::rest_after`] beyond that. A pair at
//! rest is in the store alone: what wakes it loads the end of its history,
//! from its last summary.

mod host;
mod http;
mod model;
mod provider;
mod task;

pub use host::{Config, Connection, Host, HostError, Rejected};
pub use mensor_api::{MindStatus, Prompted};
pub use model::{Model, ModelError, ModelFuture, Scripted, Wrote};
pub use provider::Provider;
pub use task::Outgoing;

//! A server that hosts minds.
//!
//! The [`Host`] is the process around the pure [`mensor_mind::Mind`]. It keeps
//! every history in a [`Store`](mensor_mind::Store), runs one task for each
//! mind that is awake, asks a [`Model`], and serves two things over HTTP:
//!
//! - **Attach**, at `GET /v1/attach`: a WebSocket a hand dials, naming its
//!   mind and itself. A mind the host does not know is started, unless its
//!   name is a fork child's, which is not made yet.
//! - **The API**, under `/v1/minds`: list minds, prompt one under a key,
//!   interrupt its turn, release one of its hands, read its history, and
//!   follow it as server-sent events from a cursor, with the text of each
//!   reply as the model writes it.
//!
//! The host performs each of a mind's outputs in order: an item is in the
//! store before anything it describes is sent, and a fork's children are
//! made before the fork is stored.
//!
//! A mind is awake while a hand is connected, someone follows it or its
//! model is asked, and for [`Config::rest_after`] beyond that. A mind at
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

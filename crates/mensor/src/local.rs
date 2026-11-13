//! A body in its mind's own process.
//!
//! The protocol is the same as over a network. Only the transport differs:
//! messages pass through memory, with no socket between the two.

use mensor_body_host::{BoxFuture, Dial, DialError, Gone, Incoming, Link};
use mensor_mind_host::{Connection, Host, Outgoing, Rejected};
use mensor_protocol::{FromBody, Hello};

/// Reaches a [`Host`] in the same process.
#[derive(Clone)]
pub struct Local {
    host: Host,
}

impl Local {
    /// A dial that connects straight to `host`.
    pub fn new(host: Host) -> Self {
        Self { host }
    }
}

impl Dial for Local {
    fn dial(&self, hello: Hello) -> BoxFuture<'_, Result<Box<dyn Link>, DialError>> {
        Box::pin(async move {
            match self.host.connect(hello).await {
                Ok(connection) => Ok(Box::new(Direct(connection)) as Box<dyn Link>),
                Err(Rejected::Refused(refusal)) => Err(DialError::Refused(refusal)),
                Err(Rejected::Failed(error)) => Err(DialError::Retry(error)),
            }
        })
    }
}

struct Direct(Connection);

impl Link for Direct {
    fn send(&mut self, message: FromBody) -> BoxFuture<'_, Result<(), Gone>> {
        let sent = self.0.send(message);
        Box::pin(async move { if sent { Ok(()) } else { Err(Gone) } })
    }

    fn receive(&mut self) -> BoxFuture<'_, Incoming> {
        Box::pin(async move {
            match self.0.next().await {
                Some(Outgoing::Message(message)) => Incoming::Message(message),
                Some(Outgoing::Close(refusal)) => Incoming::Refused(refusal),
                None => Incoming::Closed,
            }
        })
    }
}

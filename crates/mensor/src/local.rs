//! A hand in its mind's own process.
//!
//! The protocol is the same as over a network. Only the transport differs:
//! messages pass through memory, with no socket between the two.

use std::path::PathBuf;
use std::sync::Arc;

use mensor_hand::{Core, Limits};
use mensor_hand_host::capabilities::{Spawn, Started};
use mensor_hand_host::{BoxFuture, Dial, DialError, Gone, Handle, Incoming, Link, boot};
use mensor_mind_host::{Connection, Host, Outgoing, Rejected};
use mensor_protocol::{FromHand, Hello, Name};

use crate::hand::Does;

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
    fn send(&mut self, message: FromHand) -> BoxFuture<'_, Result<(), Gone>> {
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

/// Starts each clone in this process, on a task of its own, joined to
/// `host` in memory. It can do what the hand it was cloned from can.
#[derive(Clone)]
pub struct InProcess {
    host: Host,
    does: Does,
}

impl InProcess {
    /// Clones of a hand that does `does`, for minds on `host`.
    pub fn new(host: Host, does: Does) -> Self {
        Self { host, does }
    }

    /// Starts the hand `hand` of the mind `mind`, doing `does`.
    pub fn start(&self, mind: Name, hand: Name, does: &Does) -> Result<Handle, String> {
        let spawn: Arc<dyn Spawn> = Arc::new(Self {
            host: self.host.clone(),
            does: does.clone(),
        });
        let capabilities = does.capabilities(&spawn)?;
        let core = Core::new(mind.clone(), hand.clone(), boot(), Limits::default());
        let config = mensor_hand_host::Config::new(Local::new(self.host.clone()), capabilities);
        let (handle, ended) = mensor_hand_host::start(core, config);
        tokio::spawn(async move {
            match ended.await {
                Ok(Ok(())) => tracing::info!(%mind, %hand, "a hand ended"),
                Ok(Err(error)) => tracing::warn!(%mind, %hand, %error, "a hand ended"),
                Err(error) => tracing::warn!(%mind, %hand, %error, "a hand stopped"),
            }
        });
        Ok(handle)
    }
}

impl Spawn for InProcess {
    fn spawn(
        &self,
        mind: Name,
        hand: Name,
        dir: PathBuf,
    ) -> BoxFuture<'_, Result<Box<dyn Started>, String>> {
        Box::pin(async move {
            let does = self.does.of_clone(dir)?;
            let handle = self.start(mind, hand, &does)?;
            Ok(Box::new(Task(handle)) as Box<dyn Started>)
        })
    }
}

struct Task(Handle);

impl Started for Task {
    fn stop(self: Box<Self>) {
        self.0.stop();
    }
}

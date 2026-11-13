use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use mensor_body::{Capabilities, Core, Effect};
use mensor_protocol::{Call, CallId, CallResult, FromBody, Name, Refusal, Reply};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{AbortHandle, JoinHandle};
use tokio::time::{Instant, sleep_until};

use crate::transport::{Dial, DialError, Incoming, Link};

/// Asked before every dial for the name the body should have now. A clone's
/// answer differs from the name it holds, and it is renamed before it says
/// hello. `None` keeps the name.
pub type NameSource = Arc<dyn Fn() -> Option<Name> + Send + Sync>;

/// How a body runs.
#[derive(Clone)]
pub struct Config {
    /// How the body reaches its mind.
    pub dial: Arc<dyn Dial>,
    /// What the body can do.
    pub capabilities: Capabilities,
    /// Asked before every dial for the body's name.
    pub name: Option<NameSource>,
    /// How long to wait between dials.
    pub backoff: Backoff,
    /// How often to show a quiet connection is alive.
    pub ping: Duration,
}

impl Config {
    /// A body that reaches its mind through `dial`, with these capabilities.
    pub fn new(dial: impl Dial + 'static, capabilities: Capabilities) -> Self {
        Self {
            dial: Arc::new(dial),
            capabilities,
            name: None,
            backoff: Backoff::default(),
            ping: Duration::from_secs(20),
        }
    }
}

/// The wait between dials: it doubles from `min` to `max`, and each wait is
/// cut by a random part so that clones made together do not dial together.
#[derive(Clone, Copy, Debug)]
pub struct Backoff {
    /// The first wait.
    pub min: Duration,
    /// The longest wait.
    pub max: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            min: Duration::from_millis(100),
            max: Duration::from_secs(5),
        }
    }
}

impl Backoff {
    fn wait(self, attempt: u32) -> Duration {
        let doubled = self.min.saturating_mul(1 << attempt.min(16));
        let full = doubled.min(self.max);
        // The random part comes from the system each time, never from state a
        // clone would share with its original.
        let mut random = [0u8; 1];
        let cut = if getrandom::fill(&mut random).is_ok() {
            u32::from(random[0])
        } else {
            0
        };
        full.saturating_sub(full / 2 * cut / 255)
    }
}

/// Why a body gave up.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The host's address or the token cannot be used.
    #[error("the host's address or token cannot be used: {0}")]
    Config(String),
    /// The host turned the body away, and dialling again would not help.
    #[error("the mind's host refused this body: {}", .0.reason())]
    Refused(Refusal),
    /// The host did not accept the body's token.
    #[error("the mind's host did not accept this body's token")]
    Unauthorized,
}

/// A random boot for a body process that starts from nothing.
///
/// # Panics
///
/// If the system has no randomness to give.
pub fn boot() -> String {
    use std::fmt::Write;

    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("the system has randomness");
    bytes
        .iter()
        .fold(String::with_capacity(32), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

enum Control {
    Rename(Name),
    Redial,
    Snapshot(oneshot::Sender<Core>),
    Stop,
}

/// A way to reach a running body.
#[derive(Clone, Debug)]
pub struct Handle {
    control: mpsc::UnboundedSender<Control>,
}

impl Handle {
    /// Feeds the body a new name, because it is a clone. It drops its
    /// connection and says hello again under the new name.
    pub fn rename(&self, name: Name) {
        let _ = self.control.send(Control::Rename(name));
    }

    /// Drops the connection and dials again, asking the name source first.
    pub fn redial(&self) {
        let _ = self.control.send(Control::Redial);
    }

    /// A copy of what the body remembers right now. A core started from it
    /// is a clone, once it is fed a new name.
    pub async fn snapshot(&self) -> Option<Core> {
        let (reply, answer) = oneshot::channel();
        self.control.send(Control::Snapshot(reply)).ok()?;
        answer.await.ok()
    }

    /// Stops the body. Calls it is running are dropped.
    pub fn stop(&self) {
        let _ = self.control.send(Control::Stop);
    }
}

/// Starts a body on its own task.
pub fn start(core: Core, config: Config) -> (Handle, JoinHandle<Result<(), Error>>) {
    let (control, controls) = mpsc::unbounded_channel();
    let driver = Driver::new(core, config, controls);
    (Handle { control }, tokio::spawn(driver.run()))
}

struct Driver {
    core: Core,
    config: Config,
    controls: mpsc::UnboundedReceiver<Control>,
    tasks: HashMap<CallId, AbortHandle>,
    done: mpsc::UnboundedSender<(CallId, Reply)>,
    finished: mpsc::UnboundedReceiver<(CallId, Reply)>,
}

/// How a connection, or a wait for one, ended.
enum Ended {
    /// Dial again after the backoff.
    Dropped,
    /// Dial again at once.
    Renamed,
    /// The body was told to stop.
    Stopped,
    /// The host refused the body.
    Refused(Refusal),
}

impl Driver {
    fn new(mut core: Core, config: Config, controls: mpsc::UnboundedReceiver<Control>) -> Self {
        // This process runs none of the calls the core was stored or copied
        // with.
        core.interrupted();
        let (done, finished) = mpsc::unbounded_channel();
        Self {
            core,
            config,
            controls,
            tasks: HashMap::new(),
            done,
            finished,
        }
    }

    async fn run(mut self) -> Result<(), Error> {
        let mut attempt = 0;
        loop {
            let ended = match self.dial().await {
                Ok(link) => {
                    attempt = 0;
                    tracing::info!(body = %self.core.name(), "connected to the mind's host");
                    self.serve(link).await
                }
                Err(DialError::Fail(error)) => return Err(error),
                Err(DialError::Refused(refusal)) => Ended::Refused(refusal),
                Err(DialError::Retry(reason)) => {
                    tracing::debug!(body = %self.core.name(), %reason, "the dial failed");
                    Ended::Dropped
                }
            };
            let ended = match ended {
                Ended::Dropped => {
                    let wait = self.config.backoff.wait(attempt);
                    attempt += 1;
                    self.wait(wait).await
                }
                Ended::Refused(refusal) if !refusal.is_final() => {
                    tracing::info!(body = %self.core.name(), reason = refusal.reason(), "refused for now");
                    attempt += 1;
                    self.wait(self.config.backoff.max).await
                }
                other => other,
            };
            match ended {
                Ended::Stopped => return Ok(()),
                Ended::Refused(refusal) if refusal.is_final() => {
                    return Err(Error::Refused(refusal));
                }
                Ended::Dropped | Ended::Renamed | Ended::Refused(_) => {}
            }
        }
    }

    /// Asks for the body's name, then opens a connection and says hello.
    async fn dial(&mut self) -> Result<Box<dyn Link>, DialError> {
        if let Some(name) = self.config.name.as_ref().and_then(|source| source()) {
            self.core.rename(name);
        }
        let hello = self.core.hello(self.config.capabilities.declare());
        self.config.dial.dial(hello).await
    }

    /// Carries one connection until it ends.
    async fn serve(&mut self, mut link: Box<dyn Link>) -> Ended {
        let mut ping =
            tokio::time::interval_at(Instant::now() + self.config.ping, self.config.ping);
        loop {
            tokio::select! {
                incoming = link.receive() => match incoming {
                    Incoming::Message(message) => {
                        for effect in self.core.receive(message) {
                            if let Some(send) = self.perform(effect)
                                && link.send(send).await.is_err()
                            {
                                return Ended::Dropped;
                            }
                        }
                    }
                    Incoming::Refused(refusal) => return Ended::Refused(refusal),
                    Incoming::Closed => return Ended::Dropped,
                },
                Some((id, reply)) = self.finished.recv() => {
                    if let Some(result) = self.finish(id, reply)
                        && link.send(FromBody::Result(result)).await.is_err()
                    {
                        return Ended::Dropped;
                    }
                }
                control = self.controls.recv() => {
                    if let Some(ended) = self.control(control) {
                        link.close().await;
                        return ended;
                    }
                }
                _ = ping.tick() => {
                    if link.ping().await.is_err() {
                        return Ended::Dropped;
                    }
                }
            }
        }
    }

    /// Waits out a backoff. Calls that finish meanwhile are held.
    async fn wait(&mut self, wait: Duration) -> Ended {
        let until = Instant::now() + wait;
        loop {
            tokio::select! {
                () = sleep_until(until) => return Ended::Dropped,
                Some((id, reply)) = self.finished.recv() => {
                    self.finish(id, reply);
                }
                control = self.controls.recv() => {
                    if let Some(ended) = self.control(control) {
                        return ended;
                    }
                }
            }
        }
    }

    /// Does what the core asks, and returns what must be sent to the mind.
    fn perform(&mut self, effect: Effect) -> Option<FromBody> {
        match effect {
            Effect::Start(call) => self.start(call).map(FromBody::Result),
            Effect::Cancel(id) => {
                if let Some(task) = self.tasks.remove(&id) {
                    task.abort();
                }
                self.core
                    .finished(id, Reply::Error("cancelled".into()))
                    .map(FromBody::Result)
            }
            Effect::Send(message) => Some(message),
        }
    }

    /// Starts a call on its own task. A call the body cannot start finishes
    /// at once, with the error to send.
    fn start(&mut self, call: Call) -> Option<CallResult> {
        let Call {
            id,
            capability,
            operation,
            input,
        } = call;
        let Some(provider) = self.config.capabilities.get(&capability) else {
            let error = format!("this body has no capability named {capability}");
            return self.core.finished(id, Reply::Error(error));
        };
        let done = self.done.clone();
        let task = tokio::spawn(async move {
            let reply = provider.call(&operation, input).await;
            let _ = done.send((id, reply));
        });
        self.tasks.insert(id, task.abort_handle());
        None
    }

    fn finish(&mut self, id: CallId, reply: Reply) -> Option<CallResult> {
        self.tasks.remove(&id);
        self.core.finished(id, reply)
    }

    /// Handles one control message. `Some` ends the connection or the wait.
    fn control(&mut self, control: Option<Control>) -> Option<Ended> {
        match control {
            Some(Control::Rename(name)) => {
                self.core.rename(name);
                Some(Ended::Renamed)
            }
            Some(Control::Redial) => Some(Ended::Renamed),
            Some(Control::Snapshot(reply)) => {
                let _ = reply.send(self.core.clone());
                None
            }
            Some(Control::Stop) | None => {
                for (_, task) in self.tasks.drain() {
                    task.abort();
                }
                Some(Ended::Stopped)
            }
        }
    }
}

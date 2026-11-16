use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use mensor_hand::{Capabilities, Core, Effect};
use mensor_protocol::{Call, CallId, CallResult, FromHand, Hello, Refusal, Reply};
use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinHandle};
use tokio::time::{Instant, sleep_until};

use crate::transport::{Dial, DialError, Incoming, Link};

/// How a hand runs.
#[derive(Clone)]
pub struct Config {
    /// How the hand reaches its mind.
    pub dial: Arc<dyn Dial>,
    /// What the hand can do.
    pub capabilities: Capabilities,
    /// How long to wait between dials.
    pub backoff: Backoff,
    /// How often to show a quiet connection is alive.
    pub ping: Duration,
    /// How long a hand keeps dialling a mind that is not made yet before it
    /// gives up: a clone whose fork never finished.
    pub give_up: Duration,
}

impl Config {
    /// A hand that reaches its mind through `dial`, with these capabilities.
    pub fn new(dial: impl Dial + 'static, capabilities: Capabilities) -> Self {
        Self::over(Arc::new(dial), capabilities)
    }

    /// The same, through a dial other hands share.
    pub fn over(dial: Arc<dyn Dial>, capabilities: Capabilities) -> Self {
        Self {
            dial,
            capabilities,
            backoff: Backoff::default(),
            ping: Duration::from_secs(20),
            give_up: Duration::from_mins(2),
        }
    }
}

/// The wait between dials: it doubles from `min` to `max`, and each wait is
/// cut by a random part so that hands started together do not dial together.
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
        // copy of the process would share with it.
        let mut random = [0u8; 1];
        let cut = if getrandom::fill(&mut random).is_ok() {
            u32::from(random[0])
        } else {
            0
        };
        full.saturating_sub(full / 2 * cut / 255)
    }
}

/// How a hand ended, when it did not fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    /// It was told to stop.
    Stopped,
    /// Its mind let it go.
    Released,
}

/// Why a hand gave up.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The host's address or the token cannot be used.
    #[error("the host's address or token cannot be used: {0}")]
    Config(String),
    /// The host turned the hand away, and dialling again would not help.
    #[error("the mind's host refused this hand: {}", .0.reason())]
    Refused(Refusal),
    /// The host did not accept the hand's token.
    #[error("the mind's host did not accept this hand's token")]
    Unauthorized,
}

/// A random boot for a hand process that starts from nothing.
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
    Redial,
    Stop,
}

/// A way to reach a running hand.
#[derive(Clone, Debug)]
pub struct Handle {
    control: mpsc::UnboundedSender<Control>,
}

impl Handle {
    /// Drops the connection and dials again at once.
    pub fn redial(&self) {
        let _ = self.control.send(Control::Redial);
    }

    /// Stops the hand. Calls it is running are dropped. A hand whose every
    /// handle is dropped runs on, until it is refused.
    pub fn stop(&self) {
        let _ = self.control.send(Control::Stop);
    }
}

/// Starts a hand on its own task.
pub fn start(core: Core, config: Config) -> (Handle, JoinHandle<Result<Ended, Error>>) {
    let (control, controls) = mpsc::unbounded_channel();
    let driver = Driver::new(core, config, controls);
    (Handle { control }, tokio::spawn(driver.run()))
}

struct Driver {
    core: Core,
    config: Config,
    controls: mpsc::UnboundedReceiver<Control>,
    /// Whether every handle has been dropped, so no control can come.
    unheld: bool,
    tasks: HashMap<CallId, AbortHandle>,
    done: mpsc::UnboundedSender<(CallId, Reply)>,
    finished: mpsc::UnboundedReceiver<(CallId, Reply)>,
}

/// How a connection, or a wait for one, ended.
enum Closed {
    /// Dial again after the backoff.
    Dropped,
    /// Dial again at once.
    Redial,
    /// The hand was told to stop.
    Stopped,
    /// The host refused the hand.
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
            unheld: false,
            tasks: HashMap::new(),
            done,
            finished,
        }
    }

    async fn run(mut self) -> Result<Ended, Error> {
        let (mind, hand) = (self.core.mind().clone(), self.core.hand().clone());
        let mut attempt = 0;
        // Since when the host has said the mind is not made yet.
        let mut refused_since = None;
        loop {
            let hello = self.core.hello(self.config.capabilities.declare());
            let dialled = match self.dial(hello).await {
                Ok(dialled) => dialled,
                Err(Closed::Stopped) => return Ok(Ended::Stopped),
                Err(_) => continue,
            };
            let ended = match dialled {
                Ok(link) => {
                    tracing::debug!(%mind, %hand, "dialled the mind's host");
                    let ended = self.serve(link).await;
                    // A connection the host ends with a refusal was never
                    // taken: the backoff goes on.
                    if !matches!(ended, Closed::Refused(_)) {
                        attempt = 0;
                    }
                    ended
                }
                Err(DialError::Fail(error)) => return Err(error),
                Err(DialError::Refused(refusal)) => Closed::Refused(refusal),
                Err(DialError::Retry(reason)) => {
                    tracing::debug!(%mind, %hand, %reason, "the dial failed");
                    Closed::Dropped
                }
            };
            // Only refusals one after another count toward giving up: a
            // host that was away is no reason to.
            if !matches!(ended, Closed::Refused(_)) {
                refused_since = None;
            }
            let ended = match ended {
                Closed::Refused(Refusal::Released) => {
                    tracing::info!(%mind, %hand, "the mind let this hand go");
                    return Ok(Ended::Released);
                }
                Closed::Refused(refusal) if !refusal.is_final() => {
                    let since = *refused_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= self.config.give_up {
                        return Err(Error::Refused(refusal));
                    }
                    tracing::debug!(%mind, %hand, reason = refusal.reason(), "refused for now");
                    let wait = self.config.backoff.wait(attempt);
                    attempt += 1;
                    self.wait(wait).await
                }
                Closed::Dropped => {
                    let wait = self.config.backoff.wait(attempt);
                    attempt += 1;
                    self.wait(wait).await
                }
                other => other,
            };
            match ended {
                Closed::Stopped => return Ok(Ended::Stopped),
                Closed::Refused(refusal) if refusal.is_final() => {
                    return Err(Error::Refused(refusal));
                }
                Closed::Dropped | Closed::Redial | Closed::Refused(_) => {}
            }
        }
    }

    /// Dials the mind's host, until the dial ends or a control cuts it
    /// short: told to stop, a hand stops at once, however long a dial takes.
    async fn dial(&mut self, hello: Hello) -> Result<Result<Box<dyn Link>, DialError>, Closed> {
        let dial = Arc::clone(&self.config.dial);
        let dialling = dial.dial(hello);
        tokio::pin!(dialling);
        loop {
            tokio::select! {
                dialled = &mut dialling => return Ok(dialled),
                Some((id, reply)) = self.finished.recv() => {
                    self.finish(id, reply);
                }
                control = self.controls.recv(), if !self.unheld => {
                    if let Some(closed) = self.control(control.as_ref()) {
                        return Err(closed);
                    }
                }
            }
        }
    }

    /// Carries one connection until it ends.
    async fn serve(&mut self, mut link: Box<dyn Link>) -> Closed {
        let mut ping =
            tokio::time::interval_at(Instant::now() + self.config.ping, self.config.ping);
        loop {
            tokio::select! {
                incoming = link.receive() => match incoming {
                    Incoming::Message(message) => {
                        // Calls that ended first are told first, so that a
                        // cancel never overtakes a call that has ended.
                        while let Ok((id, reply)) = self.finished.try_recv() {
                            if let Some(result) = self.finish(id, reply)
                                && link.send(FromHand::Result(result)).await.is_err()
                            {
                                return Closed::Dropped;
                            }
                        }
                        for effect in self.core.receive(message) {
                            if let Some(send) = self.perform(effect)
                                && link.send(send).await.is_err()
                            {
                                return Closed::Dropped;
                            }
                        }
                    }
                    Incoming::Refused(refusal) => return Closed::Refused(refusal),
                    Incoming::Closed => return Closed::Dropped,
                },
                Some((id, reply)) = self.finished.recv() => {
                    if let Some(result) = self.finish(id, reply)
                        && link.send(FromHand::Result(result)).await.is_err()
                    {
                        return Closed::Dropped;
                    }
                }
                control = self.controls.recv(), if !self.unheld => {
                    if let Some(ended) = self.control(control.as_ref()) {
                        link.close().await;
                        return ended;
                    }
                }
                _ = ping.tick() => {
                    if link.ping().await.is_err() {
                        return Closed::Dropped;
                    }
                }
            }
        }
    }

    /// Waits out a backoff. Calls that finish meanwhile are held.
    async fn wait(&mut self, wait: Duration) -> Closed {
        let until = Instant::now() + wait;
        loop {
            tokio::select! {
                () = sleep_until(until) => return Closed::Dropped,
                Some((id, reply)) = self.finished.recv() => {
                    self.finish(id, reply);
                }
                control = self.controls.recv(), if !self.unheld => {
                    if let Some(ended) = self.control(control.as_ref()) {
                        return ended;
                    }
                }
            }
        }
    }

    /// Does what the core asks, and returns what must be sent to the mind.
    fn perform(&mut self, effect: Effect) -> Option<FromHand> {
        match effect {
            Effect::Start(call) => self.start(call).map(FromHand::Result),
            Effect::Cancel(id) => {
                if let Some(task) = self.tasks.remove(&id) {
                    task.abort();
                }
                self.core
                    .finished(id, Reply::Error("cancelled".into()))
                    .map(FromHand::Result)
            }
            Effect::Send(message) => Some(message),
        }
    }

    /// Starts a call on its own task. A call the hand cannot start finishes
    /// at once, with the error to send.
    fn start(&mut self, call: Call) -> Option<CallResult> {
        let Call {
            id,
            capability,
            operation,
            input,
        } = call;
        let Some(provider) = self.config.capabilities.get(&capability) else {
            let error = format!("this hand has no capability named {capability}");
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
    fn control(&mut self, control: Option<&Control>) -> Option<Closed> {
        match control {
            Some(Control::Redial) => Some(Closed::Redial),
            None => {
                self.unheld = true;
                None
            }
            Some(Control::Stop) => {
                for (_, task) in self.tasks.drain() {
                    task.abort();
                }
                Some(Closed::Stopped)
            }
        }
    }
}

impl Drop for Driver {
    /// A hand that ends, released or refused, stops what it runs. In a
    /// process of its own that ends with it anyway; in its host's process
    /// it would not.
    fn drop(&mut self) {
        for (_, task) in self.tasks.drain() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use mensor_hand::{Core, Limits};
    use mensor_protocol::Name;

    use super::*;
    use crate::BoxFuture;

    /// A host that never answers a dial.
    struct Silent;

    impl Dial for Silent {
        fn dial(&self, _: Hello) -> BoxFuture<'_, Result<Box<dyn Link>, DialError>> {
            Box::pin(std::future::pending())
        }
    }

    #[tokio::test]
    async fn a_hand_told_to_stop_stops_while_it_dials() {
        let core = Core::new(
            Name::new("m").unwrap(),
            Name::new("a1").unwrap(),
            boot(),
            Limits::default(),
        );
        let (handle, ended) = start(core, Config::new(Silent, Capabilities::new()));
        tokio::task::yield_now().await;
        handle.stop();
        let ended = tokio::time::timeout(Duration::from_secs(5), ended).await;
        assert!(matches!(ended, Ok(Ok(Ok(Ended::Stopped)))), "{ended:?}");
    }
}

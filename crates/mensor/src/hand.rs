//! Running hands: the flags, and the hands they describe.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use clap::Args;
use mensor_hand::{Capabilities, Core, Limits};
use mensor_hand_host::capabilities::{Cloning, Commands, Disk, Files, Spawn, Started, copies};
use mensor_hand_host::{BoxFuture, Config, Dial, Ended, Error, Handle, WebSocket, boot};
use mensor_protocol::{Name, Refusal};
use tokio::sync::watch;

use crate::{CREDENTIALS, stopped, token};

/// A hand that dials a mind's host.
#[derive(Args)]
pub struct Hand {
    /// Where hands attach on the mind's host.
    #[arg(long, default_value = "ws://127.0.0.1:7878/v1/attach")]
    url: String,
    /// The mind this hand joins. A name its host does not know starts one.
    #[arg(long)]
    mind: Name,
    /// This hand's name in its mind.
    #[arg(long = "hand")]
    name: Name,
    #[command(flatten)]
    does: Does,
}

/// What a hand can do, and where.
#[derive(Args, Clone)]
pub struct Does {
    /// The directory commands run in and relative paths start from.
    #[arg(long, default_value = ".")]
    dir: PathBuf,
    /// Where the hand's clones are made: a directory per mind, and in it a
    /// copy per clone. `<dir>.clones` unless set. Clones run in this
    /// process, and starting it again starts one on each copy.
    #[arg(long)]
    clones: Option<PathBuf>,
    /// What the hand can do: `commands`, `files` and `clone`.
    #[arg(long, value_delimiter = ',', default_value = "commands,files,clone")]
    capabilities: Vec<String>,
}

impl Does {
    /// A hand that works in `dir`, can do `capabilities`, and makes its
    /// clones in `clones`, or beside `dir` when that is `None`.
    pub fn new(dir: impl Into<PathBuf>, clones: Option<PathBuf>, capabilities: &[&str]) -> Self {
        Self {
            dir: dir.into(),
            clones,
            capabilities: capabilities.iter().map(|&name| name.to_owned()).collect(),
        }
    }

    /// The directory the hand works in, made absolute.
    pub fn dir(&self) -> Result<PathBuf, String> {
        std::fs::canonicalize(&self.dir)
            .map_err(|error| format!("{} cannot be used: {error}", self.dir.display()))
    }

    /// Where the hand's clones are made, as an absolute path.
    pub fn clones(&self) -> Result<PathBuf, String> {
        if let Some(clones) = &self.clones {
            return std::path::absolute(clones)
                .map_err(|error| format!("{} cannot be used: {error}", clones.display()));
        }
        let dir = self.dir()?;
        Ok(match dir.file_name() {
            Some(name) => {
                let mut name = name.to_os_string();
                name.push(".clones");
                dir.with_file_name(name)
            }
            None => dir.join(".clones"),
        })
    }

    /// The capabilities asked for. `clone` starts each clone through
    /// `spawn`.
    pub fn capabilities(&self, spawn: &Arc<dyn Spawn>) -> Result<Capabilities, String> {
        let dir = self.dir()?;
        let mut capabilities = Capabilities::new();
        for capability in &self.capabilities {
            capabilities = match capability.as_str() {
                "commands" => capabilities.with(Commands::new(&dir).hide(CREDENTIALS)),
                "files" => capabilities.with(Files::new(Disk::new(&dir))),
                "clone" => capabilities.with(Cloning::new(&dir, self.clones()?, Arc::clone(spawn))),
                other => return Err(format!("this hand has no capability named {other}")),
            };
        }
        Ok(capabilities)
    }

    /// The same, for a clone working in `dir`: its clones go where this
    /// hand's do.
    pub fn of_clone(&self, dir: PathBuf) -> Result<Self, String> {
        Ok(Self {
            dir,
            clones: Some(self.clones()?),
            capabilities: self.capabilities.clone(),
        })
    }
}

/// Runs a hand that dials a mind's host, and its clones, until every one
/// has ended or the process is asked to stop.
pub async fn run(hand: Hand) -> Result<(), String> {
    let dial = WebSocket::new(hand.url).token(token());
    let hands = Hands::new(dial, hand.does);
    hands.start(hand.mind, hand.name)?;
    tokio::select! {
        ended = hands.ended() => ended,
        () = stopped() => {
            hands.stop();
            hands.ended().await
        }
    }
}

/// The hands one process runs: one on a directory, and every clone made
/// from it, each on a task of its own.
///
/// A clone works in a copy of its original's directory, kept under the
/// clones directory as `<clones>/<mind>/<hand>.<code>`. The copies are what
/// the clones are: [`Hands::start`] starts a hand on each copy whose mind
/// shares the root of the one it starts, and a copy is removed once its
/// mind lets its clone go, or its clone gives up on a mind its fork never
/// made. A copy whose clone is stopped is kept, to be started again.
#[derive(Clone)]
pub struct Hands {
    inner: Arc<Inner>,
}

struct Inner {
    dial: Arc<dyn Dial>,
    does: Does,
    /// How long a clone dials a mind not made yet before it gives up.
    give_up: Option<Duration>,
    /// Each hand that runs, by a number of its own.
    running: Mutex<BTreeMap<u64, Handle>>,
    next: AtomicU64,
    /// How many hands run.
    count: watch::Sender<usize>,
    /// How the first hand started ended, once it has.
    first: Mutex<Option<Result<(), String>>>,
}

impl Hands {
    /// Hands that reach their minds through `dial`, and do what `does`
    /// says: the one started works in its directory, and each clone in a
    /// copy of it.
    pub fn new(dial: impl Dial + 'static, does: Does) -> Self {
        Self {
            inner: Arc::new(Inner {
                dial: Arc::new(dial),
                does,
                give_up: None,
                running: Mutex::default(),
                next: AtomicU64::new(0),
                count: watch::Sender::new(0),
                first: Mutex::default(),
            }),
        }
    }

    /// The same, with clones that give up on a mind not made yet after
    /// `give_up`, not two minutes.
    pub fn giving_up(dial: impl Dial + 'static, does: Does, give_up: Duration) -> Self {
        let mut hands = Self::new(dial, does);
        if let Some(inner) = Arc::get_mut(&mut hands.inner) {
            inner.give_up = Some(give_up);
        }
        hands
    }

    /// Starts the hand `hand` of the mind `mind` in the directory, then one
    /// on each copy a clone of a mind of the same root left.
    pub fn start(&self, mind: Name, hand: Name) -> Result<(), String> {
        let does = &self.inner.does;
        let clones = does.clones()?;
        let found = copies(&clones, mind.root())
            .map_err(|error| format!("{} cannot be read: {error}", clones.display()))?;
        self.run(mind, hand, does, None)?;
        for copy in found {
            tracing::info!(mind = %copy.mind, hand = %copy.hand, dir = %copy.dir.display(), "a clone starts again");
            let of_clone = does.of_clone(copy.dir.clone())?;
            self.run(copy.mind, copy.hand, &of_clone, Some(copy.dir))?;
        }
        Ok(())
    }

    /// Stops every hand. A clone's copy is kept.
    pub fn stop(&self) {
        let running = self
            .inner
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for handle in running.values() {
            handle.stop();
        }
    }

    /// Waits until every hand has ended, and says how the first one
    /// started did.
    pub async fn ended(&self) -> Result<(), String> {
        let mut count = self.inner.count.subscribe();
        let _ = count.wait_for(|count| *count == 0).await;
        let first = self
            .inner
            .first
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        first.clone().unwrap_or(Ok(()))
    }

    /// Starts one hand doing what `does` says. `copy` is the copy a clone
    /// works in, removed once the clone is done with it.
    fn run(
        &self,
        mind: Name,
        hand: Name,
        does: &Does,
        copy: Option<PathBuf>,
    ) -> Result<Handle, String> {
        let spawn: Arc<dyn Spawn> = Arc::new(self.clone());
        let capabilities = does.capabilities(&spawn)?;
        let core = Core::new(mind.clone(), hand.clone(), boot(), Limits::default());
        let mut config = Config::over(Arc::clone(&self.inner.dial), capabilities);
        if let Some(give_up) = self.inner.give_up {
            config.give_up = give_up;
        }
        let (handle, task) = mensor_hand_host::start(core, config);
        let id = self.inner.next.fetch_add(1, Ordering::Relaxed);
        self.inner
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, handle.clone());
        self.inner.count.send_modify(|count| *count += 1);
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            let ended = task.await.unwrap_or_else(|error| {
                Err(Error::Config(format!("the hand's task failed: {error}")))
            });
            match &ended {
                Ok(how) => tracing::info!(%mind, %hand, ?how, "a hand ended"),
                Err(error) => tracing::warn!(%mind, %hand, %error, "a hand ended"),
            }
            // A clone its mind let go, or whose mind was never made, is done
            // with its copy.
            let done = matches!(
                ended,
                Ok(Ended::Released) | Err(Error::Refused(Refusal::UnknownMind))
            );
            if let Some(copy) = copy.filter(|_| done) {
                let removed =
                    tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&copy)).await;
                if let Ok(Err(error)) = removed {
                    tracing::warn!(%mind, %hand, %error, "a clone's copy could not be removed");
                }
            }
            inner
                .running
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id);
            if id == 0 {
                *inner.first.lock().unwrap_or_else(PoisonError::into_inner) =
                    Some(ended.map(|_| ()).map_err(|error| error.to_string()));
            }
            inner.count.send_modify(|count| *count -= 1);
        });
        Ok(handle)
    }
}

impl Spawn for Hands {
    fn spawn(
        &self,
        mind: Name,
        hand: Name,
        dir: PathBuf,
    ) -> BoxFuture<'_, Result<Box<dyn Started>, String>> {
        Box::pin(async move {
            let does = self.inner.does.of_clone(dir.clone())?;
            let handle = self.run(mind.clone(), hand.clone(), &does, Some(dir))?;
            tracing::info!(%mind, %hand, "a clone was started");
            Ok(Box::new(Spawned(handle)) as Box<dyn Started>)
        })
    }
}

/// A clone a [`Hands`] started.
struct Spawned(Handle);

impl Started for Spawned {
    fn stop(self: Box<Self>) {
        self.0.stop();
    }
}

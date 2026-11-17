//! One mind and its hand as one Durable Object.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::Duration;

use futures_channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures_util::future::{Either, select};
use futures_util::{StreamExt, stream};
use mensor_api::{History, MindStatus, PAGE, Problem, PromptBody, PromptReply, Prompted};
use mensor_hand::{Capabilities, Files};
use mensor_mind::{MindRecord, MindSettings, Settings};
use mensor_object::{LOCAL, Object, PromptError, StorageError, step};
use mensor_protocol::Name;
// The object's attribute expands to code that names this crate.
use worker::wasm_bindgen;
use worker::{
    Date, DurableObject, Env, Request, Response, Result, State, console_error, durable_object,
    js_sys,
};

use crate::api::{self, Route};
use crate::provider::Provider;
use crate::sql::{self, SqlFiles, SqlStore};
use crate::web::Web;
use crate::{json, problem};

/// How long after an object starts work it is woken again if that work
/// never ended, because the object was stopped under it. `MENSOR_RECOVERY_MS`
/// sets another.
const RECOVERY: Duration = Duration::from_secs(30);

/// How many pieces of work one wake does before it sets the alarm and
/// returns, so that no wake outgrows the requests the platform allows one.
const STEPS_PER_WAKE: usize = 32;

/// How long a wake goes on starting new work. The platform interrupts an alarm
/// after fifteen minutes, and a model request may take four
/// ([`crate::provider::TIMEOUT`]), so a wake that starts nothing after eight
/// ends in time. One that is stopped anyway carries on as after any stop.
const WAKE: Duration = Duration::from_mins(8);

type Shared = Rc<RefCell<Object<SqlStore>>>;

/// One mind: its history, its one hand and the hand's files, in this
/// object's database.
///
/// The alarm is the object's only worker. A prompt is stored and sets the
/// alarm; the alarm asks the model and runs the hand's calls until the mind
/// is idle. An object that is stopped mid-work wakes on the same alarm and
/// carries on from what it stored.
#[durable_object]
pub(crate) struct Mind {
    state: State,
    env: Env,
    capabilities: Capabilities,
    /// The mind, once a request or an alarm has needed it.
    object: RefCell<Option<Shared>>,
    /// Whether an alarm is doing the object's work now.
    working: Cell<bool>,
    /// Whether this object has set an alarm that is still to fire, so that
    /// it need not set another.
    armed: Cell<bool>,
    /// Those following the mind, each told what happens in its order.
    watchers: RefCell<Vec<UnboundedSender<Told>>>,
    /// Tells the alarm that is working to give up what it waits for.
    stopper: RefCell<Option<UnboundedSender<()>>>,
}

impl DurableObject for Mind {
    fn new(state: State, env: Env) -> Self {
        let storage = state.storage();
        if let Err(error) = sql::prepare(&storage.sql()) {
            console_error!("the object's tables could not be made: {error}");
        }
        let capabilities = Capabilities::new()
            .with(Files::new(SqlFiles::new(storage.sql())))
            .with(Web::default());
        Self {
            state,
            env,
            capabilities,
            object: RefCell::new(None),
            working: Cell::new(false),
            armed: Cell::new(false),
            watchers: RefCell::new(Vec::new()),
            stopper: RefCell::new(None),
        }
    }

    async fn fetch(&self, request: Request) -> Result<Response> {
        let url = request.url()?;
        let route = match api::route(request.method().as_ref(), url.path()) {
            Ok(route) => route,
            Err(refused) => return problem(&refused),
        };
        let served = match route {
            Route::Prompt(name) => self.prompt(name, request).await,
            Route::Interrupt(name) => self.interrupt(&name).await,
            Route::Status(name) => self.status(&name).await,
            Route::History(name) => self.history(&name, url.query()).await,
            Route::Events(name) => self.events(&name, url.query()).await,
        };
        match served {
            Ok(response) => Ok(response),
            Err(Unserved::Problem(refused)) => problem(&refused),
            Err(Unserved::Platform(error)) => Err(error),
        }
    }

    async fn alarm(&self) -> Result<Response> {
        self.armed.set(false);
        match self.work().await {
            Ok(()) => Response::empty(),
            Err(Unserved::Problem(refused)) => {
                // The platform runs a failed alarm again, a little later.
                console_error!("the object's work stopped: {}", refused.detail);
                Err(refused.detail.into())
            }
            Err(Unserved::Platform(error)) => Err(error),
        }
    }
}

/// What those following a mind are told.
#[derive(Clone)]
enum Told {
    /// The history grew.
    Grew,
    /// The model wrote this of a reply that is not stored yet: a `writing`
    /// event.
    Wrote(Vec<u8>),
}

/// Why a request was not served.
enum Unserved {
    Problem(Problem),
    Platform(worker::Error),
}

impl From<worker::Error> for Unserved {
    fn from(error: worker::Error) -> Self {
        Self::Platform(error)
    }
}

impl From<Problem> for Unserved {
    fn from(refused: Problem) -> Self {
        Self::Problem(refused)
    }
}

impl From<StorageError> for Unserved {
    fn from(error: StorageError) -> Self {
        Self::Problem(Problem::new(
            500,
            format!("the history could not be stored: {error}"),
        ))
    }
}

impl Mind {
    fn store(&self) -> SqlStore {
        SqlStore::new(self.state.storage().sql())
    }

    /// What the mind is told: `MENSOR_SYSTEM`, and `MENSOR_COMPACT_AT` in
    /// tokens, where 0 never sums a conversation up.
    fn settings(&self) -> Settings {
        let var = |name: &str| self.env.var(name).ok().map(|value| value.to_string());
        let mut settings = Settings {
            own: MindSettings {
                system: var("MENSOR_SYSTEM").filter(|system| !system.is_empty()),
                ..MindSettings::default()
            },
            ..Settings::default()
        };
        if let Some(tokens) = var("MENSOR_COMPACT_AT").and_then(|tokens| tokens.parse().ok()) {
            settings.compact_at = (tokens > 0).then_some(tokens);
        }
        settings
    }

    fn recovery(&self) -> Duration {
        let set = self.env.var("MENSOR_RECOVERY_MS").ok();
        set.and_then(|millis| millis.to_string().parse().ok())
            .map_or(RECOVERY, Duration::from_millis)
    }

    /// The mind stored here, if one has started.
    fn open(&self) -> std::result::Result<Option<Shared>, StorageError> {
        if let Some(object) = self.object.borrow().clone() {
            return Ok(Some(object));
        }
        let opened = Object::open(self.capabilities.declare(), self.settings(), self.store())?;
        Ok(opened.map(|object| self.hold(object)))
    }

    /// The mind stored here, or a new one named `name`.
    fn start(&self, name: Name) -> std::result::Result<Shared, StorageError> {
        if let Some(object) = self.object.borrow().clone() {
            return Ok(object);
        }
        let declared = self.capabilities.declare();
        let object = Object::start(name, boot, declared, self.settings(), self.store())?;
        Ok(self.hold(object))
    }

    fn hold(&self, object: Object<SqlStore>) -> Shared {
        let object = Rc::new(RefCell::new(object));
        self.object.replace(Some(Rc::clone(&object)));
        object
    }

    /// Lets go of the mind in memory after its storage failed, so the next
    /// request reads what was really stored. Those following it are cut
    /// off, and follow again from their cursors.
    fn forget(&self) {
        self.object.replace(None);
        self.watchers.borrow_mut().clear();
    }

    /// The mind `name` asks about, which must be the one stored here.
    fn known(&self, name: &Name) -> std::result::Result<Shared, Unserved> {
        match self.open()? {
            Some(object) if object.borrow().name() == name => Ok(object),
            _ => Err(Problem::new(404, format!("no mind is named {name}")).into()),
        }
    }

    /// Tells everyone following the history that it grew.
    fn publish(&self) {
        self.tell(&Told::Grew);
    }

    /// Tells everyone following the mind, and forgets those who have gone.
    fn tell(&self, told: &Told) {
        self.watchers
            .borrow_mut()
            .retain(|watcher| watcher.unbounded_send(told.clone()).is_ok());
    }

    /// Tells everyone following the mind the text its model has written.
    fn wrote(&self, at: u64, text: &str) {
        if !self.watchers.borrow().is_empty() {
            self.tell(&Told::Wrote(api::writing(at, text)));
        }
    }

    /// Sets the alarm for now if the mind has work, and no alarm is doing
    /// it or is set to.
    async fn wake(&self, object: &Shared) -> Result<()> {
        if object.borrow().has_work() && !self.working.get() && !self.armed.replace(true) {
            self.state.storage().set_alarm(Duration::ZERO).await?;
        }
        Ok(())
    }

    async fn prompt(
        &self,
        name: Name,
        mut request: Request,
    ) -> std::result::Result<Response, Unserved> {
        let Ok(body) = request.json::<PromptBody>().await else {
            let detail = "a prompt is JSON with a text, and may have a key";
            return Err(Problem::new(400, detail).into());
        };
        // An object's mind starts from nothing, so it has a root name.
        if !name.is_root() {
            let detail =
                "a mind that starts from nothing has no '.' in its name, and at most 55 bytes";
            return Err(Problem::new(400, detail).into());
        }
        let object = self.start(name)?;
        let prompted = object.borrow_mut().prompt(body.key, body.text);
        let existing = match prompted {
            Ok(existing) => existing,
            Err(PromptError::Conflict(conflict)) => {
                return Err(Problem::new(409, conflict.to_string()).into());
            }
            Err(PromptError::Storage(error)) => {
                self.forget();
                return Err(error.into());
            }
        };
        self.publish();
        self.wake(&object).await?;
        let status = if existing {
            Prompted::Existing
        } else {
            Prompted::Accepted
        };
        Ok(json(status.status(), &PromptReply { status })?)
    }

    /// Stops the mind's turn, and whatever the alarm waits on for it.
    async fn interrupt(&self, name: &Name) -> std::result::Result<Response, Unserved> {
        let object = self.known(name)?;
        let interrupted = object.borrow_mut().interrupt();
        if let Err(error) = interrupted {
            self.forget();
            return Err(error.into());
        }
        if let Some(stopper) = &*self.stopper.borrow() {
            let _ = stopper.unbounded_send(());
        }
        self.publish();
        self.status(name).await
    }

    async fn status(&self, name: &Name) -> std::result::Result<Response, Unserved> {
        let object = self.known(name)?;
        self.wake(&object).await?;
        let object = object.borrow();
        // A mind here is never a fork, its one hand is always with it, and
        // so its turn waits for no hand.
        let status = MindStatus {
            mind: MindRecord::root(object.name().clone()),
            state: object.state(),
            hands: vec![Name::new(LOCAL).map_err(|error| Problem::new(500, error.to_string()))?],
            waiting: Vec::new(),
            watching: Vec::new(),
            length: object.length(),
        };
        Ok(json(200, &status)?)
    }

    async fn history(
        &self,
        name: &Name,
        query: Option<&str>,
    ) -> std::result::Result<Response, Unserved> {
        let after = api::after(query)?;
        let object = self.known(name)?;
        self.wake(&object).await?;
        let items = object.borrow().read(after, PAGE)?;
        Ok(json(200, &History::from(after, &items))?)
    }

    /// The history as server-sent events: every item from the cursor, then
    /// each new one as it is stored. An event's id is its item's index.
    /// Between them come `writing` events, with no id: the text of the
    /// reply the model is writing.
    async fn events(
        &self,
        name: &Name,
        query: Option<&str>,
    ) -> std::result::Result<Response, Unserved> {
        let after = api::after(query)?;
        let object = self.known(name)?;
        self.wake(&object).await?;
        let (watcher, told) = unbounded();
        self.watchers.borrow_mut().push(watcher);

        let events = stream::unfold((object, after, told, true, VecDeque::new()), follow);
        let mut response = Response::from_stream(events)?;
        let headers = response.headers_mut();
        headers.set("content-type", "text/event-stream")?;
        headers.set("cache-control", "no-store")?;
        Ok(response)
    }

    /// Does the mind's work until none is left, or this wake has done its
    /// share.
    async fn work(&self) -> std::result::Result<(), Unserved> {
        let Some(object) = self.open()? else {
            return Ok(());
        };
        if self.working.replace(true) {
            return Ok(());
        }
        let storage = self.state.storage();
        if let Err(error) = storage.set_alarm(self.recovery()).await {
            self.working.set(false);
            return Err(error.into());
        }
        self.armed.set(true);

        let model = Provider::from_env(&self.env, |at, text| self.wrote(at, text));
        let (stopper, mut interrupts) = unbounded();
        self.stopper.replace(Some(stopper));
        let began = Date::now().as_millis();
        let mut steps = 0;
        let more = loop {
            let spent = Duration::from_millis(Date::now().as_millis().saturating_sub(began));
            if steps == STEPS_PER_WAKE || spent > WAKE {
                break Ok(object.borrow().has_work());
            }
            // An interrupt that came between two pieces of work has nothing
            // of this wake's to stop.
            while interrupts.try_recv().is_ok() {}
            let stepping = std::pin::pin!(step(&object, &self.capabilities, &model));
            let stepped = match select(stepping, interrupts.next()).await {
                Either::Left((stepped, _)) => stepped,
                // The turn was interrupted: the object has answered for this
                // work itself, so the wake interrupts waiting for it.
                Either::Right(_) => Ok(true),
            };
            match stepped {
                Ok(true) => {
                    steps += 1;
                    self.publish();
                }
                Ok(false) => break Ok(false),
                Err(error) => break Err(error),
            }
        };
        self.stopper.replace(None);
        self.working.set(false);
        match more {
            Ok(true) => storage.set_alarm(Duration::ZERO).await?,
            Ok(false) => {
                // Cleared before the await, so that a prompt which arrives
                // during it sets an alarm of its own.
                self.armed.set(false);
                storage.delete_alarm().await?;
            }
            Err(error) => {
                self.forget();
                return Err(error.into());
            }
        }
        Ok(())
    }
}

/// A followed mind: the index of the next item to read, what the mind
/// tells, whether the history may hold that item, and the events read and
/// not yet sent.
type Following = (
    Shared,
    u64,
    UnboundedReceiver<Told>,
    bool,
    VecDeque<Vec<u8>>,
);

/// The next event of a followed mind, once there is one. The stream ends
/// when the object lets go of the mind, or its history cannot be read.
///
/// The history is read a page at a time: at the start, and then only when
/// it is told to have grown. What is told comes in the order it happened,
/// so the text of a reply always comes before the step that holds it.
async fn follow(
    (object, mut next, mut told, mut grew, mut ready): Following,
) -> Option<(Result<Vec<u8>>, Following)> {
    loop {
        if let Some(event) = ready.pop_front() {
            return Some((Ok(event), (object, next, told, grew, ready)));
        }
        if grew {
            let page = object.borrow().read(next, PAGE).ok()?;
            // A full page may not be the last.
            grew = page.len() == PAGE;
            for item in &page {
                ready.push_back(api::event(next, item));
                next += 1;
            }
            continue;
        }
        match told.next().await? {
            Told::Grew => grew = true,
            Told::Wrote(event) => return Some((Ok(event), (object, next, told, grew, ready))),
        }
    }
}

/// A string no other hand has. It marks one hand's memory, and guards
/// nothing, so the platform's ordinary random numbers are enough.
fn boot() -> String {
    // The 52 random bits of each number the platform draws.
    let half = || js_sys::Math::random().to_bits() & ((1 << 52) - 1);
    format!("{:013x}{:013x}", half(), half())
}

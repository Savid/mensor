//! The threads that hold the store's connections.
//!
//! Each connection belongs to one thread, which runs what it is sent on it,
//! in turn. A caller sends its work and waits for the answer, so the store
//! may be called from any thread, a runtime's included: the client's own
//! runtime only ever runs on its thread.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread::JoinHandle;

use mensor_mind::StoreError;
use mensor_protocol::Name;
use postgres::error::Severity;
use postgres::{Client, Statement};

use crate::connect::Target;

/// One connection, made again when it is lost.
///
/// The writer's also holds the schema for this host: each new connection
/// takes the advisory lock before anything is written, and checks that no
/// other host took the schema while this one did not hold it. One did if
/// the count in `hold` is no longer the one this store set when it first
/// took it: what this host has in memory may be out of date, so its store
/// writes nothing more.
pub(crate) struct Session {
    target: Arc<Target>,
    holds: bool,
    client: Option<Client>,
    statements: HashMap<&'static str, Statement>,
    /// The count in `hold` this store set, once it has.
    taken: Option<i64>,
    /// Whether another host held the schema in between.
    fenced: bool,
}

/// What the writer's session is set to before it takes the schema:
/// commits wait for the disk whatever the database's default, and the
/// server lets go of a host gone without a word within about a minute,
/// and with it the lock.
const WRITER: &str = "
    SET synchronous_commit = on;
    SET tcp_keepalives_idle = 30;
    SET tcp_keepalives_interval = 10;
    SET tcp_keepalives_count = 3;
    SET tcp_user_timeout = 60000;
";

/// Takes the schema for this host, unless another host holds it: one per
/// schema, for as long as the connection that took it lasts.
const HOLD: &str =
    "SELECT pg_try_advisory_lock(hashtextextended('mensor ' || coalesce(current_schema(), ''), 0))";

impl Session {
    pub(crate) fn new(target: Arc<Target>, holds: bool) -> Self {
        Self {
            target,
            holds,
            client: None,
            statements: HashMap::new(),
            taken: None,
            fenced: false,
        }
    }

    /// The connection, made if there is none.
    pub(crate) fn client(&mut self) -> Result<&mut Client, StoreError> {
        if self.fenced {
            return Err(fenced());
        }
        let client = match self.client.take() {
            Some(client) if !client.is_closed() => client,
            _ => {
                self.statements.clear();
                let mut client = self.target.connect().map_err(|error| failed(&error))?;
                if self.holds {
                    self.hold(&mut client)?;
                }
                client
            }
        };
        Ok(self.client.insert(client))
    }

    /// Takes the schema on a new connection of the writer's, and checks
    /// that nobody else took it since this store first did.
    fn hold(&mut self, client: &mut Client) -> Result<(), StoreError> {
        let failed = |error: postgres::Error| failed(&error);
        client.batch_execute(WRITER).map_err(failed)?;
        if !client
            .query_one(HOLD, &[])
            .map_err(failed)?
            .get::<_, bool>(0)
        {
            return Err(StoreError::Backend(
                "another mensor host holds this Postgres database: one host keeps its \
                 histories at a time"
                    .into(),
            ));
        }
        if let Some(taken) = self.taken {
            let now: i64 = client
                .query_one("SELECT taken FROM hold", &[])
                .map_err(failed)?
                .get(0);
            if now != taken {
                self.fenced = true;
                return Err(fenced());
            }
        }
        Ok(())
    }

    /// Marks the schema as this store's, once its tables are there: the
    /// count in `hold` goes up, and this store remembers it.
    pub(crate) fn take(&mut self) -> Result<(), StoreError> {
        let taken = self.with(|client| {
            client.query_one("UPDATE hold SET taken = taken + 1 RETURNING taken", &[])
        })?;
        self.taken = Some(taken.get(0));
        Ok(())
    }

    /// `sql` prepared on the connection, once for each connection.
    pub(crate) fn statement(&mut self, sql: &'static str) -> Result<Statement, StoreError> {
        self.client()?;
        if let Some(statement) = self.statements.get(sql) {
            return Ok(statement.clone());
        }
        let prepared = self.client()?.prepare(sql);
        let prepared = self.lost(prepared)?;
        self.statements.insert(sql, prepared.clone());
        Ok(prepared)
    }

    /// Runs `work` on the connection. One the work found lost is made again
    /// for the next.
    pub(crate) fn with<T>(
        &mut self,
        work: impl FnOnce(&mut Client) -> Result<T, postgres::Error>,
    ) -> Result<T, StoreError> {
        let done = work(self.client()?);
        self.lost(done)
    }

    /// Whether the session has a connection: none after one was lost.
    pub(crate) fn connected(&self) -> bool {
        self.client.is_some()
    }

    /// `done` as a store's error. The connection is let go unless the
    /// error was an ordinary one of SQL's, which leaves it as it was: the
    /// server may have ended it before the client has seen it close.
    fn lost<T>(&mut self, done: Result<T, postgres::Error>) -> Result<T, StoreError> {
        done.map_err(|error| {
            let ordinary = error.as_db_error().is_some_and(|db| {
                !matches!(
                    db.parsed_severity(),
                    Some(Severity::Fatal | Severity::Panic) | None
                )
            });
            if !ordinary || self.client.as_ref().is_none_or(Client::is_closed) {
                self.client = None;
                self.statements.clear();
            }
            failed(&error)
        })
    }
}

type Job = Box<dyn FnOnce(&mut Session) + Send>;

/// Commits a batch of appends, and says how each ended.
pub(crate) type Commit = fn(&mut Session, &[Append]) -> Vec<Result<(), StoreError>>;

/// A thread with a connection, which runs one job at a time.
pub(crate) struct Reader {
    jobs: Option<Sender<Job>>,
    thread: Option<JoinHandle<()>>,
}

impl Reader {
    pub(crate) fn start(mut session: Session) -> Self {
        let (jobs, received) = mpsc::channel::<Job>();
        let thread = std::thread::Builder::new()
            .name("mensor-postgres-read".into())
            .spawn(move || {
                for job in received {
                    job(&mut session);
                }
            })
            .ok();
        Self {
            jobs: Some(jobs),
            thread,
        }
    }

    /// Runs `work` on the thread's connection, and waits for it. Work that
    /// found its connection lost runs once more, on a new one: it must be
    /// work that may run twice.
    pub(crate) fn run<T: Send + 'static>(
        &self,
        work: impl Fn(&mut Session) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T, StoreError> {
        let (answer, answered) = mpsc::sync_channel(1);
        let job: Job = Box::new(move |session| {
            let _ = answer.send(again(session, &work));
        });
        let sent = self.jobs.as_ref().map(|jobs| jobs.send(job));
        match sent {
            Some(Ok(())) => answered.recv().map_err(|_| stopped())?,
            _ => Err(stopped()),
        }
    }
}

impl Drop for Reader {
    /// Ends the thread, and with it its connection, before returning.
    fn drop(&mut self) {
        drop(self.jobs.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// One append, waiting for the commit that stores it.
pub(crate) struct Append {
    pub(crate) name: Name,
    pub(crate) rows: Vec<crate::Row>,
    pub(crate) answer: SyncSender<Result<(), StoreError>>,
}

enum Write {
    Append(Append),
    Job(Job),
}

/// The thread with the connection that writes, and holds the database.
///
/// Appends that arrive while it commits wait together, and share the next
/// commit: the minds of a busy host pay for one round trip to the database
/// between them, not one each.
pub(crate) struct Writer {
    writes: Option<Sender<Write>>,
    thread: Option<JoinHandle<()>>,
}

impl Writer {
    /// Starts the thread, which commits batches of appends with `commit`.
    pub(crate) fn start(mut session: Session, commit: Commit) -> Self {
        let (writes, received) = mpsc::channel::<Write>();
        let thread = std::thread::Builder::new()
            .name("mensor-postgres-write".into())
            .spawn(move || write(&mut session, &received, commit))
            .ok();
        Self {
            writes: Some(writes),
            thread,
        }
    }

    /// Runs `work` on the connection that writes, and waits for it. Work
    /// that found its connection lost runs once more, on a new one: it must
    /// be work that may run twice.
    pub(crate) fn run<T: Send + 'static>(
        &self,
        work: impl Fn(&mut Session) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T, StoreError> {
        let (answer, answered) = mpsc::sync_channel(1);
        let job: Job = Box::new(move |session| {
            let _ = answer.send(again(session, &work));
        });
        self.send(Write::Job(job))?;
        answered.recv().map_err(|_| stopped())?
    }

    /// Stores `rows` at the end of a mind's history, and returns once the
    /// commit that holds them has.
    pub(crate) fn append(&self, name: &Name, rows: Vec<crate::Row>) -> Result<(), StoreError> {
        let (answer, answered) = mpsc::sync_channel(1);
        self.send(Write::Append(Append {
            name: name.clone(),
            rows,
            answer,
        }))?;
        answered.recv().map_err(|_| stopped())?
    }

    fn send(&self, write: Write) -> Result<(), StoreError> {
        let writes = self.writes.as_ref().ok_or_else(stopped)?;
        writes.send(write).map_err(|_| stopped())
    }
}

impl Drop for Writer {
    /// Ends the thread, and with it the connection that holds the
    /// database, before returning: another host may take it then.
    fn drop(&mut self) {
        drop(self.writes.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The writer's thread: each job in turn, and the appends that wait
/// together in one commit.
fn write(session: &mut Session, received: &Receiver<Write>, commit: Commit) {
    let mut next = None;
    loop {
        let write = match next.take() {
            Some(write) => write,
            None => match received.recv() {
                Ok(write) => write,
                Err(_) => return,
            },
        };
        match write {
            Write::Job(job) => job(session),
            Write::Append(first) => {
                let mut batch = vec![first];
                while let Ok(write) = received.try_recv() {
                    match write {
                        Write::Append(append) => batch.push(append),
                        job @ Write::Job(_) => {
                            next = Some(job);
                            break;
                        }
                    }
                }
                for (append, ended) in batch.iter().zip(commit(session, &batch)) {
                    let _ = append.answer.send(ended);
                }
            }
        }
    }
}

/// `work`, run once more on a new connection if it found its own lost.
fn again<T>(
    session: &mut Session,
    work: impl Fn(&mut Session) -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    match work(session) {
        Err(_) if !session.connected() => work(session),
        done => done,
    }
}

pub(crate) fn backend(error: impl std::fmt::Display) -> StoreError {
    StoreError::Backend(error.to_string())
}

/// A client's error, with every cause it carries: its own words name only
/// the kind of thing that failed.
pub(crate) fn failed(error: &postgres::Error) -> StoreError {
    let mut words = error.to_string();
    let mut cause = std::error::Error::source(error);
    while let Some(next) = cause {
        words.push_str(": ");
        words.push_str(&next.to_string());
        cause = next.source();
    }
    StoreError::Backend(words)
}

fn fenced() -> StoreError {
    StoreError::Backend(
        "another mensor host took this Postgres database while this one's connection was \
         lost, so this host stores nothing more: start it again to carry on from what is stored"
            .into(),
    )
}

fn stopped() -> StoreError {
    StoreError::Backend("the store's connection thread has stopped".into())
}

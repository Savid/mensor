//! A mind's histories in one `SQLite` file.
//!
//! Each item is one row, keyed by its mind and its index in that mind's
//! history. A fork stores only its own items: the ones it shares stay rows of
//! its parent, and a read walks up the lineage for them. An append commits
//! with `synchronous = FULL`, so it is on disk when it returns.
//!
//! A commit waits for the disk, and one commit runs at a time. So appends
//! that arrive while one is being committed wait together and share the
//! next: the minds of a busy host pay for one wait on the disk between
//! them, not one each.
//!
//! A file is opened twice: once to write, and once more only to read. A
//! read then sees everything committed, and waits for no commit.
//!
//! Beside the item itself a row holds when it was stored, its type, and the
//! key of a prompt that has one. Those are what a reader of the file asks
//! by, and how the store finds a history's last summary and a prompt's key
//! without reading the history.

use std::path::Path;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use mensor_mind::{Item, MindRecord, Origin, Store, StoreError};
use mensor_protocol::Name;
use rusqlite::{Connection, OptionalExtension, params};

const SETTINGS: &str = "
    PRAGMA journal_mode = WAL;
    PRAGMA synchronous = FULL;
    PRAGMA foreign_keys = ON;
    PRAGMA busy_timeout = 5000;
";

const SCHEMA: &str = "
    BEGIN;
    CREATE TABLE minds (
        made      INTEGER PRIMARY KEY AUTOINCREMENT,
        name      TEXT NOT NULL UNIQUE,
        parent    TEXT REFERENCES minds (name),
        inherited INTEGER NOT NULL
    ) STRICT;
    CREATE TABLE items (
        mind TEXT NOT NULL REFERENCES minds (name),
        at   INTEGER NOT NULL,
        time INTEGER NOT NULL, -- when it was stored, in ms since 1970
        kind TEXT NOT NULL,    -- prompt, step, call, outcome and so on
        key  TEXT,             -- of a prompt that has one
        item TEXT NOT NULL,
        PRIMARY KEY (mind, at)
    ) STRICT, WITHOUT ROWID;
    CREATE INDEX summaries ON items (mind, at) WHERE kind = 'summary';
    CREATE INDEX knowing ON items (mind, at)
        WHERE kind IN ('summary', 'attached', 'cloned', 'released', 'forked', 'parted');
    CREATE INDEX keys ON items (mind, key) WHERE key IS NOT NULL;
    COMMIT;
";

/// A [`Store`] in one `SQLite` database.
#[derive(Debug)]
pub struct SqliteStore {
    /// The connection that writes.
    connection: Mutex<Connection>,
    /// A second connection to a file, which only reads. A database in
    /// memory has the one connection, and reads through that.
    reader: Option<Mutex<Connection>>,
    /// The appends that wait for a commit, and how those before them ended.
    appends: Mutex<Appends>,
    /// Signalled when a commit has ended.
    committed: Condvar,
}

/// The appends of every thread, between their arrival and their return.
#[derive(Debug, Default)]
struct Appends {
    /// How many appends have arrived: the ticket of the last.
    arrived: u64,
    /// Those no commit has taken yet, oldest first.
    waiting: Vec<Waiting>,
    /// Whether a thread is committing now.
    committing: bool,
    /// How each append a commit took has ended, until its thread reads it.
    ended: Vec<(u64, Result<(), StoreError>)>,
}

/// One append that waits for a commit: its ticket, its mind and its rows.
#[derive(Debug)]
struct Waiting {
    ticket: u64,
    name: Name,
    rows: Vec<Row>,
}

/// One item as its row holds it.
#[derive(Debug)]
struct Row {
    kind: &'static str,
    key: Option<String>,
    item: String,
}

impl SqliteStore {
    /// Opens the database at `path`, making it if it is not there.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref();
        let mut store = Self::prepare(Connection::open(path).map_err(backend)?)?;
        let reader = Connection::open(path).map_err(backend)?;
        reader
            .execute_batch("PRAGMA query_only = ON;")
            .map_err(backend)?;
        store.reader = Some(Mutex::new(reader));
        Ok(store)
    }

    /// A database in memory, which nothing outlives.
    pub fn in_memory() -> Result<Self, StoreError> {
        Self::prepare(Connection::open_in_memory().map_err(backend)?)
    }

    /// Makes the tables in a new file. A file that holds other tables is
    /// refused: its rows are not these.
    fn prepare(connection: Connection) -> Result<Self, StoreError> {
        connection.execute_batch(SETTINGS).map_err(backend)?;
        let count = |sql: &str| -> Result<i64, StoreError> {
            connection
                .query_row(sql, [], |row| row.get(0))
                .map_err(backend)
        };
        let tables = count("SELECT COUNT(*) FROM sqlite_schema")?;
        let minds = count("SELECT COUNT(*) FROM sqlite_schema WHERE name = 'minds'")?;
        match (tables, minds) {
            (0, _) => connection.execute_batch(SCHEMA).map_err(backend)?,
            (_, 1) => {}
            _ => {
                return Err(StoreError::Backend(
                    "the file holds tables this store does not: start from a new file".into(),
                ));
            }
        }
        Ok(Self {
            connection: Mutex::new(connection),
            reader: None,
            appends: Mutex::default(),
            committed: Condvar::new(),
        })
    }

    /// The connection that writes.
    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The connection to read through.
    fn reading(&self) -> MutexGuard<'_, Connection> {
        match &self.reader {
            Some(reader) => reader.lock().unwrap_or_else(PoisonError::into_inner),
            None => self.lock(),
        }
    }

    /// Commits a batch of appends as one, and says how each ended. An
    /// append to a mind the store does not hold is refused by itself. If
    /// the database fails, nothing of the batch is stored, and every
    /// append in it is told so.
    fn commit(&self, batch: &[Waiting]) -> Vec<(u64, Result<(), StoreError>)> {
        let mut ended = Vec::with_capacity(batch.len());
        match self.insert(batch, &mut ended) {
            Ok(()) => ended,
            Err(error) => {
                let failed = error.to_string();
                let told =
                    |waiting: &Waiting| (waiting.ticket, Err(StoreError::Backend(failed.clone())));
                batch.iter().map(told).collect()
            }
        }
    }

    fn insert(
        &self,
        batch: &[Waiting],
        ended: &mut Vec<(u64, Result<(), StoreError>)>,
    ) -> Result<(), StoreError> {
        let now = now()?;
        let mut connection = self.lock();
        let transaction = connection.transaction().map_err(backend)?;
        {
            let mut insert = transaction
                .prepare_cached(
                    "INSERT INTO items (mind, at, time, kind, key, item) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )
                .map_err(backend)?;
            for waiting in batch {
                let length = match length(&transaction, &waiting.name) {
                    Ok(length) => length,
                    Err(missing @ StoreError::Missing(_)) => {
                        ended.push((waiting.ticket, Err(missing)));
                        continue;
                    }
                    Err(failed) => return Err(failed),
                };
                for (at, row) in (length..).zip(&waiting.rows) {
                    let name = waiting.name.as_str();
                    let written = params![name, signed(at)?, now, row.kind, row.key, row.item];
                    insert.execute(written).map_err(backend)?;
                }
                ended.push((waiting.ticket, Ok(())));
            }
        }
        transaction.commit().map_err(backend)
    }
}

/// The rows `items` are stored as.
fn rows(items: &[Item]) -> Result<Vec<Row>, StoreError> {
    items
        .iter()
        .map(|item| {
            Ok(Row {
                kind: item.kind(),
                key: item.key().map(str::to_owned),
                item: serde_json::to_string(item).map_err(backend)?,
            })
        })
        .collect()
}

/// Now, in ms since 1970, as a row's time.
fn now() -> Result<i64, StoreError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(backend)?;
    i64::try_from(now.as_millis()).map_err(backend)
}

/// A mind's parent and the number of items it shares with it.
fn origin(connection: &Connection, name: &Name) -> Result<Origin, StoreError> {
    let row: Option<(Option<String>, i64)> = connection
        .query_row(
            "SELECT parent, inherited FROM minds WHERE name = ?1",
            params![name.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(backend)?;
    match row {
        None => Err(StoreError::Missing(name.clone())),
        Some((None, _)) => Ok(Origin::Root),
        Some((Some(parent), inherited)) => Ok(Origin::Fork {
            parent: stored_name(parent)?,
            inherited: unsigned(inherited)?,
        }),
    }
}

/// How many items a mind's history holds, shared ones included.
fn length(connection: &Connection, name: &Name) -> Result<u64, StoreError> {
    let inherited = match origin(connection, name)? {
        Origin::Root => 0,
        Origin::Fork { inherited, .. } => inherited,
    };
    let last: Option<i64> = connection
        .query_row(
            "SELECT MAX(at) FROM items WHERE mind = ?1",
            params![name.as_str()],
            |row| row.get(0),
        )
        .map_err(backend)?;
    match last {
        Some(last) => Ok(unsigned(last)? + 1),
        None => Ok(inherited),
    }
}

/// The lineage of a mind, youngest first: each mind in it with the indices
/// of the history that are its own rows, from the items it inherited up to
/// the items its fork inherited from it.
fn lineage(connection: &Connection, name: &Name) -> Result<Vec<(Name, u64, u64)>, StoreError> {
    let mut segments = Vec::new();
    let (mut mind, mut end) = (name.clone(), u64::MAX);
    loop {
        match origin(connection, &mind)? {
            Origin::Root => {
                segments.push((mind, 0, end));
                return Ok(segments);
            }
            Origin::Fork { parent, inherited } => {
                segments.push((mind, inherited.min(end), end));
                end = end.min(inherited);
                mind = parent;
            }
        }
    }
}

/// The last item in a lineage that `filter` holds for. `source` is `items`,
/// with the index to search by if the planner must be told, `filter` is
/// SQL over it, and `value` is its `?4` if it has one.
fn last(
    connection: &Connection,
    lineage: &[(Name, u64, u64)],
    (source, filter): (&str, &str),
    value: Option<&str>,
) -> Result<Option<Item>, StoreError> {
    let query = format!(
        "SELECT item FROM {source} WHERE mind = ?1 AND at >= ?2 AND at < ?3 AND {filter} \
         ORDER BY at DESC LIMIT 1"
    );
    let mut select = connection.prepare_cached(&query).map_err(backend)?;
    for (mind, start, end) in lineage {
        let (start, end) = (signed(*start)?, i64::try_from(*end).unwrap_or(i64::MAX));
        let row = |row: &rusqlite::Row<'_>| row.get::<_, String>(0);
        let found = match value {
            Some(value) => select.query_row(params![mind.as_str(), start, end, value], row),
            None => select.query_row(params![mind.as_str(), start, end], row),
        };
        if let Some(found) = found.optional().map_err(backend)? {
            return serde_json::from_str(&found).map(Some).map_err(backend);
        }
    }
    Ok(None)
}

/// The items of a lineage from index `from` that say what its mind knows of
/// its hands and kin, each with its index.
fn known(
    connection: &Connection,
    lineage: &[(Name, u64, u64)],
    from: u64,
) -> Result<Vec<(u64, Item)>, StoreError> {
    let mut select = connection
        .prepare_cached(
            "SELECT at, item FROM items WHERE mind = ?1 AND at >= ?2 AND at < ?3 \
             AND kind IN ('summary', 'attached', 'cloned', 'released', 'forked', 'parted') \
             ORDER BY at",
        )
        .map_err(backend)?;
    let mut known = Vec::new();
    for (mind, start, end) in lineage.iter().rev() {
        let start = (*start).max(from);
        if start >= *end {
            continue;
        }
        let end = i64::try_from(*end).unwrap_or(i64::MAX);
        let rows = select
            .query_map(params![mind.as_str(), signed(start)?, end], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(backend)?;
        for row in rows {
            let (at, item) = row.map_err(backend)?;
            let item = serde_json::from_str(&item).map_err(backend)?;
            known.push((unsigned(at)?, item));
        }
    }
    Ok(known)
}

/// The items of a lineage from index `from`, `limit` of them at the most.
fn items(
    connection: &Connection,
    lineage: &[(Name, u64, u64)],
    from: u64,
    limit: usize,
) -> Result<Vec<Item>, StoreError> {
    let mut select = connection
        .prepare_cached(
            "SELECT item FROM items WHERE mind = ?1 AND at >= ?2 AND at < ?3 ORDER BY at LIMIT ?4",
        )
        .map_err(backend)?;
    let mut items = Vec::new();
    for (mind, start, end) in lineage.iter().rev() {
        let start = (*start).max(from);
        let left = i64::try_from(limit - items.len()).unwrap_or(i64::MAX);
        if start >= *end || left == 0 {
            continue;
        }
        let end = i64::try_from(*end).unwrap_or(i64::MAX);
        let rows = select
            .query_map(params![mind.as_str(), signed(start)?, end, left], |row| {
                row.get::<_, String>(0)
            })
            .map_err(backend)?;
        for row in rows {
            items.push(serde_json::from_str(&row.map_err(backend)?).map_err(backend)?);
        }
    }
    Ok(items)
}

impl Store for SqliteStore {
    fn create(&self, mind: &MindRecord, items: &[Item]) -> Result<(), StoreError> {
        let rows = rows(items)?;
        let now = now()?;
        let mut connection = self.lock();
        let transaction = connection.transaction().map_err(backend)?;
        match origin(&transaction, &mind.name) {
            Ok(origin) if origin == mind.origin => return Ok(()),
            Ok(_) => return Err(StoreError::Exists(mind.name.clone())),
            Err(StoreError::Missing(_)) => {}
            Err(failed) => return Err(failed),
        }
        let (parent, inherited) = match &mind.origin {
            Origin::Root => (None, 0),
            Origin::Fork { parent, inherited } => {
                if length(&transaction, parent)? < *inherited {
                    return Err(StoreError::Short {
                        parent: parent.clone(),
                        inherited: *inherited,
                    });
                }
                (Some(parent.as_str()), *inherited)
            }
        };
        transaction
            .execute(
                "INSERT INTO minds (name, parent, inherited) VALUES (?1, ?2, ?3)",
                params![mind.name.as_str(), parent, signed(inherited)?],
            )
            .map_err(backend)?;
        {
            let mut insert = transaction
                .prepare_cached(
                    "INSERT INTO items (mind, at, time, kind, key, item) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )
                .map_err(backend)?;
            for (at, row) in (inherited..).zip(&rows) {
                let written = params![
                    mind.name.as_str(),
                    signed(at)?,
                    now,
                    row.kind,
                    row.key,
                    row.item
                ];
                insert.execute(written).map_err(backend)?;
            }
        }
        transaction.commit().map_err(backend)
    }

    fn append(&self, name: &Name, items: &[Item]) -> Result<(), StoreError> {
        let rows = rows(items);
        let mut appends = self.appends.lock().unwrap_or_else(PoisonError::into_inner);
        appends.arrived += 1;
        let ticket = appends.arrived;
        appends.waiting.push(Waiting {
            ticket,
            name: name.clone(),
            rows: rows?,
        });
        loop {
            // A commit that took this append has ended.
            if let Some(at) = appends.ended.iter().position(|(ended, _)| *ended == ticket) {
                return appends.ended.swap_remove(at).1;
            }
            if appends.committing {
                let woken = self.committed.wait(appends);
                appends = woken.unwrap_or_else(PoisonError::into_inner);
                continue;
            }
            // Nobody is committing, so this thread does: everything that
            // waits, its own append among it, in one commit.
            appends.committing = true;
            let batch = std::mem::take(&mut appends.waiting);
            drop(appends);
            let ended = self.commit(&batch);
            appends = self.appends.lock().unwrap_or_else(PoisonError::into_inner);
            appends.ended.extend(ended);
            appends.committing = false;
            self.committed.notify_all();
        }
    }

    fn read(&self, name: &Name, from: u64, limit: usize) -> Result<Vec<Item>, StoreError> {
        let connection = self.reading();
        items(&connection, &lineage(&connection, name)?, from, limit)
    }

    fn last(&self, name: &Name, kind: &str) -> Result<Option<Item>, StoreError> {
        let connection = self.reading();
        let lineage = lineage(&connection, name)?;
        // A summary is found by the index of them, however long the
        // history. Any other kind is searched for from the end.
        if kind == "summary" {
            let indexed = ("items INDEXED BY summaries", "kind = 'summary'");
            return last(&connection, &lineage, indexed, None);
        }
        last(&connection, &lineage, ("items", "kind = ?4"), Some(kind))
    }

    fn known(&self, name: &Name) -> Result<Vec<(u64, Item)>, StoreError> {
        let connection = self.reading();
        let lineage = lineage(&connection, name)?;
        let indexed = ("items INDEXED BY summaries", "kind = 'summary'");
        let from = match last(&connection, &lineage, indexed, None)? {
            Some(Item::Summary { from, .. }) => from,
            _ => 0,
        };
        known(&connection, &lineage, from)
    }

    fn prompt(&self, name: &Name, key: &str) -> Result<Option<String>, StoreError> {
        let connection = self.reading();
        let lineage = lineage(&connection, name)?;
        let indexed = ("items INDEXED BY keys", "key = ?4");
        Ok(match last(&connection, &lineage, indexed, Some(key))? {
            Some(Item::Prompt { text, .. }) => Some(text),
            _ => None,
        })
    }

    fn minds(&self) -> Result<Vec<MindRecord>, StoreError> {
        let connection = self.reading();
        let mut select = connection
            .prepare_cached("SELECT name, parent, inherited FROM minds ORDER BY made")
            .map_err(backend)?;
        let rows = select
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .map_err(backend)?;
        let mut minds = Vec::new();
        for row in rows {
            let (name, parent, inherited) = row.map_err(backend)?;
            let origin = match parent {
                None => Origin::Root,
                Some(parent) => Origin::Fork {
                    parent: stored_name(parent)?,
                    inherited: unsigned(inherited)?,
                },
            };
            minds.push(MindRecord {
                name: stored_name(name)?,
                origin,
            });
        }
        Ok(minds)
    }
}

fn backend(error: impl std::fmt::Display) -> StoreError {
    StoreError::Backend(error.to_string())
}

fn stored_name(name: String) -> Result<Name, StoreError> {
    Name::new(name).map_err(backend)
}

fn signed(index: u64) -> Result<i64, StoreError> {
    i64::try_from(index).map_err(backend)
}

fn unsigned(index: i64) -> Result<u64, StoreError> {
    u64::try_from(index).map_err(backend)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> Name {
        Name::new(s).unwrap()
    }

    fn prompt(text: &str) -> Item {
        Item::Prompt {
            key: None,
            text: text.into(),
            from: None,
        }
    }

    fn texts(items: &[Item]) -> Vec<&str> {
        items
            .iter()
            .map(|item| match item {
                Item::Prompt { text, .. } => text.as_str(),
                _ => panic!("not a prompt"),
            })
            .collect()
    }

    fn record(mind: &str, origin: Origin) -> MindRecord {
        MindRecord {
            name: name(mind),
            origin,
        }
    }

    fn root(store: &SqliteStore, mind: &str, items: &[&str]) {
        store.create(&record(mind, Origin::Root), &[]).unwrap();
        let items: Vec<_> = items.iter().map(|text| prompt(text)).collect();
        store.append(&name(mind), &items).unwrap();
    }

    mensor_mind::store_suite!(SqliteStore::in_memory().unwrap());

    fn fork(store: &SqliteStore, mind: &str, parent: &str, inherited: u64) {
        let origin = Origin::Fork {
            parent: name(parent),
            inherited,
        };
        store.create(&record(mind, origin), &[]).unwrap();
    }

    #[test]
    fn a_row_says_when_it_was_stored_and_what_it_is() {
        let store = SqliteStore::in_memory().unwrap();
        root(&store, "a1", &["0"]);
        let (time, kind): (i64, String) = store
            .lock()
            .query_row("SELECT time, kind FROM items", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        // After 2025, in milliseconds.
        assert!(time > 1_735_689_600_000, "{time}");
        assert_eq!(kind, "prompt");
    }

    #[test]
    fn a_read_of_a_file_sees_every_append_and_waits_for_no_writer() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(dir.path().join("mensor.sqlite")).unwrap();
        root(&store, "a1", &["0"]);
        for index in 1..20 {
            store
                .append(&name("a1"), &[prompt(&index.to_string())])
                .unwrap();
            let read = store.read(&name("a1"), 0, usize::MAX).unwrap();
            assert_eq!(read.len(), index + 1);
        }
        fork(&store, "b7", "a1", 5);
        assert_eq!(store.minds().unwrap().len(), 2);

        // While something holds the connection that writes, as a commit
        // does for as long as the disk takes, a read is still answered.
        let writing = store.lock();
        assert_eq!(store.read(&name("b7"), 0, usize::MAX).unwrap().len(), 5);
        assert_eq!(store.tail(&name("a1")).unwrap().1.len(), 20);
        drop(writing);

        // The connection that reads writes nothing.
        let refused = store.reading().execute("DELETE FROM items", []);
        assert!(refused.is_err());
    }

    #[test]
    fn a_file_that_holds_other_tables_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.sqlite");
        let old = Connection::open(&path).unwrap();
        old.execute_batch("CREATE TABLE items (owner TEXT, at INTEGER, item TEXT)")
            .unwrap();
        drop(old);
        let refused = SqliteStore::open(&path).unwrap_err();
        assert!(refused.to_string().contains("new file"), "{refused}");
    }

    #[test]
    fn a_fork_stores_only_its_own_rows() {
        let store = SqliteStore::in_memory().unwrap();
        root(&store, "a1", &["0", "1", "2"]);
        fork(&store, "b7", "a1", 2);
        store.append(&name("b7"), &[prompt("b2")]).unwrap();
        let own: i64 = store
            .lock()
            .query_row("SELECT COUNT(*) FROM items WHERE mind = 'b7'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(own, 1);
        assert_eq!(
            texts(&store.read(&name("b7"), 0, usize::MAX).unwrap()),
            ["0", "1", "b2"]
        );
    }

    #[test]
    fn everything_is_there_when_the_file_is_opened_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mensor.sqlite");
        {
            let store = SqliteStore::open(&path).unwrap();
            root(&store, "a1", &["0", "1"]);
            fork(&store, "b7", "a1", 1);
            store.append(&name("b7"), &[prompt("b1")]).unwrap();
        }
        let store = SqliteStore::open(&path).unwrap();
        assert_eq!(store.minds().unwrap().len(), 2);
        assert_eq!(
            texts(&store.read(&name("b7"), 0, usize::MAX).unwrap()),
            ["0", "b1"]
        );
        store.append(&name("b7"), &[prompt("b2")]).unwrap();
        assert_eq!(
            texts(&store.read(&name("b7"), 1, usize::MAX).unwrap()),
            ["b1", "b2"]
        );
    }
}

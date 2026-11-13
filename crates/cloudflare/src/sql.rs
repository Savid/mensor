//! The object's `SQLite` database: its history, what its body remembers, and
//! its files.
//!
//! The database's writes are synchronous, and the platform commits every
//! write made between two awaits together. So an item and the work it
//! describes are stored as one whenever nothing waits between them.

use mensor_body::{Core, FileSystem, FsFuture};
use mensor_mind::Item;
use mensor_object::{Storage, StorageError, Stored, rows};
use worker::{Date, SqlStorage, SqlStorageValue};

/// The tables, made when an object is first constructed. Beside an item
/// its row holds when it was stored, its type, and the key of a prompt
/// that has one.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS items (
    at INTEGER PRIMARY KEY, time INTEGER NOT NULL, kind TEXT NOT NULL, key TEXT, item TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS summaries ON items (at) WHERE kind = 'summary';
CREATE INDEX IF NOT EXISTS keys ON items (key) WHERE key IS NOT NULL;
CREATE TABLE IF NOT EXISTS body (id INTEGER PRIMARY KEY CHECK (id = 0), core TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS files (path TEXT PRIMARY KEY, content BLOB NOT NULL);
";

/// Makes the tables.
pub(crate) fn prepare(sql: &SqlStorage) -> Result<(), String> {
    run(sql, SCHEMA, Vec::new()).map(drop)
}

/// Runs one statement to its end, and returns its rows.
fn run(
    sql: &SqlStorage,
    query: &str,
    bindings: Vec<SqlStorageValue>,
) -> Result<Vec<Vec<SqlStorageValue>>, String> {
    let cursor = sql
        .exec(query, bindings)
        .map_err(|error| error.to_string())?;
    cursor
        .raw()
        .collect::<Result<_, _>>()
        .map_err(|error| error.to_string())
}

fn text(value: Option<SqlStorageValue>) -> Result<String, String> {
    match value {
        Some(SqlStorageValue::String(text)) => Ok(text),
        other => Err(format!("a column that holds text held {other:?}")),
    }
}

/// One item, from the first column of a row.
fn item(row: Vec<SqlStorageValue>) -> Result<Item, StorageError> {
    let item = text(row.into_iter().next()).map_err(StorageError)?;
    serde_json::from_str(&item)
        .map_err(|error| StorageError(format!("a stored item is not one: {error}")))
}

/// The history and the body's memory, in the `items` and `body` tables.
pub(crate) struct SqlStore {
    sql: SqlStorage,
    length: i64,
}

impl SqlStore {
    pub(crate) fn new(sql: SqlStorage) -> Self {
        Self { sql, length: 0 }
    }

    /// The items a query selects.
    fn items(
        &self,
        query: &str,
        bindings: Vec<SqlStorageValue>,
    ) -> Result<Vec<Item>, StorageError> {
        let rows = run(&self.sql, query, bindings).map_err(StorageError)?;
        rows.into_iter().map(item).collect()
    }
}

impl Storage for SqlStore {
    fn load(&mut self) -> Result<Stored, StorageError> {
        let last = run(&self.sql, "SELECT MAX(at) FROM items", Vec::new()).map_err(StorageError)?;
        self.length = match last.into_iter().flatten().next() {
            Some(SqlStorageValue::Integer(last)) => last + 1,
            _ => 0,
        };
        let summary = self.items(
            "SELECT item FROM items WHERE kind = 'summary' ORDER BY at DESC LIMIT 1",
            Vec::new(),
        )?;
        let from = match summary.first() {
            Some(Item::Summary { from, .. }) => *from,
            _ => 0,
        };
        let items = self.read(from, usize::MAX)?;

        let body = run(&self.sql, "SELECT core FROM body", Vec::new())
            .map_err(StorageError)?
            .into_iter()
            .next();
        let body = match body {
            Some(row) => {
                let core = text(row.into_iter().next()).map_err(StorageError)?;
                let core: Core = serde_json::from_str(&core).map_err(|error| {
                    StorageError(format!("the stored body is not one: {error}"))
                })?;
                Some(core)
            }
            None => None,
        };
        Ok(Stored { from, items, body })
    }

    fn append(&mut self, items: &[Item]) -> Result<(), StorageError> {
        let now = i64::try_from(Date::now().as_millis()).unwrap_or(i64::MAX);
        for item in items {
            let written = serde_json::to_string(item)
                .map_err(|error| StorageError(format!("an item could not be written: {error}")))?;
            let key = item.key().map_or(SqlStorageValue::Null, Into::into);
            run(
                &self.sql,
                "INSERT INTO items (at, time, kind, key, item) VALUES (?, ?, ?, ?, ?)",
                vec![
                    self.length.into(),
                    now.into(),
                    item.kind().into(),
                    key,
                    written.into(),
                ],
            )
            .map_err(StorageError)?;
            self.length += 1;
        }
        Ok(())
    }

    fn read(&self, from: u64, limit: usize) -> Result<Vec<Item>, StorageError> {
        let from = i64::try_from(from).unwrap_or(i64::MAX);
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.items(
            "SELECT item FROM items WHERE at >= ? ORDER BY at LIMIT ?",
            vec![from.into(), limit.into()],
        )
    }

    fn prompt(&self, key: &str) -> Result<Option<String>, StorageError> {
        let found = self.items(
            "SELECT item FROM items WHERE key = ? LIMIT 1",
            vec![key.into()],
        )?;
        Ok(match found.into_iter().next() {
            Some(Item::Prompt { text, .. }) => Some(text),
            _ => None,
        })
    }

    fn keep(&mut self, body: &Core) -> Result<(), StorageError> {
        let written = serde_json::to_string(body)
            .map_err(|error| StorageError(format!("the body could not be written: {error}")))?;
        run(
            &self.sql,
            "INSERT INTO body (id, core) VALUES (0, ?) \
             ON CONFLICT (id) DO UPDATE SET core = excluded.core",
            vec![written.into()],
        )
        .map(drop)
        .map_err(StorageError)
    }
}

/// The body's files, one row each in the `files` table.
pub(crate) struct SqlFiles {
    sql: SqlStorage,
}

impl SqlFiles {
    pub(crate) fn new(sql: SqlStorage) -> Self {
        Self { sql }
    }

    fn read_now(&self, path: &str) -> Result<Vec<u8>, String> {
        let key = rows::key(path)?;
        let row = run(
            &self.sql,
            "SELECT content FROM files WHERE path = ?",
            vec![key.into()],
        )?
        .into_iter()
        .next();
        match row.and_then(|row| row.into_iter().next()) {
            Some(SqlStorageValue::Blob(bytes)) => Ok(bytes),
            Some(SqlStorageValue::String(text)) => Ok(text.into_bytes()),
            Some(other) => Err(format!("its row holds {other:?}")),
            None => Err("no such file".into()),
        }
    }

    fn write_now(&self, path: &str, content: Vec<u8>) -> Result<(), String> {
        let key = rows::key(path)?;
        if key.is_empty() {
            return Err("it is the directory files start from".into());
        }
        run(
            &self.sql,
            "INSERT INTO files (path, content) VALUES (?, ?) \
             ON CONFLICT (path) DO UPDATE SET content = excluded.content",
            vec![key.into(), content.into()],
        )
        .map(drop)
    }

    fn list_now(&self, path: &str) -> Result<Vec<String>, String> {
        let directory = rows::key(path)?;
        // Every key under a directory starts with its key and a slash, and
        // so sorts before the same key followed by the character after one.
        let rows = if directory.is_empty() {
            run(&self.sql, "SELECT path FROM files", Vec::new())?
        } else {
            run(
                &self.sql,
                "SELECT path FROM files WHERE path >= ? AND path < ?",
                vec![
                    format!("{directory}/").into(),
                    format!("{directory}0").into(),
                ],
            )?
        };
        let keys: Vec<String> = rows
            .into_iter()
            .map(|row| text(row.into_iter().next()))
            .collect::<Result<_, _>>()?;
        rows::children(&directory, keys.iter().map(String::as_str))
            .ok_or_else(|| "no such directory".into())
    }
}

impl FileSystem for SqlFiles {
    fn read(&self, path: &str) -> FsFuture<'_, Vec<u8>> {
        let read = self.read_now(path);
        Box::pin(async move { read })
    }

    fn write(&self, path: &str, content: Vec<u8>) -> FsFuture<'_, ()> {
        let wrote = self.write_now(path, content);
        Box::pin(async move { wrote })
    }

    fn list(&self, path: &str) -> FsFuture<'_, Vec<String>> {
        let listed = self.list_now(path);
        Box::pin(async move { listed })
    }
}

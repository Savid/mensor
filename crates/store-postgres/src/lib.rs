//! A mind's histories in one Postgres database.
//!
//! The rows are those of the `SQLite` store: each item is one row, keyed by
//! its mind and its index in that mind's history, and a fork stores only its
//! own items, reading the ones it shares from its parent's rows.
//!
//! - **One host.** The connection that writes takes an advisory lock for the
//!   database's schema, and holds it for the host's life. A second host is
//!   refused at open, and a host whose connection was lost takes the lock
//!   again before it writes, or fails.
//! - **Group commit.** Appends that arrive while one commits wait together
//!   and share the next commit, which is one round trip for all of them. An
//!   append returns once its commit has.
//! - **Two connections, each on a thread of its own.** One writes and one
//!   reads, so a read waits for no commit. Each thread runs what it is sent
//!   in turn, so the store may be called from any thread.
//! - **Exact items.** An item is kept as the JSON it was written as, in a
//!   `json` column: `jsonb` would refuse a NUL a command printed, and change
//!   what it did not refuse. A prompt's key is kept as its UTF-8, for the
//!   same reason.
//!
//! How a database is named and reached, TLS included, is in `connect`.

mod connect;
mod tls;
mod worker;

use std::collections::HashMap;
use std::sync::Arc;

use mensor_mind::{Item, Latest, MindRecord, MindSettings, Store, StoreError};
use mensor_protocol::Name;

use crate::connect::Target;
use crate::worker::{Append, Reader, Session, Writer, backend};

/// The tables of a new database.
const SCHEMA: &str = "
    CREATE TABLE settings (
        digest   text PRIMARY KEY, -- MindSettings::digest
        settings json NOT NULL
    );
    CREATE TABLE minds (
        made      bigint GENERATED ALWAYS AS IDENTITY,
        name      text PRIMARY KEY,
        parent    text REFERENCES minds (name), -- whose items it shares
        inherited bigint NOT NULL,             -- and how many
        record    json NOT NULL,               -- the mind's record, without
        settings  text NOT NULL REFERENCES settings (digest) -- its settings
    );
    CREATE TABLE items (
        mind text        NOT NULL REFERENCES minds (name),
        at   bigint      NOT NULL,
        time timestamptz NOT NULL DEFAULT now(), -- when it was stored
        kind text        NOT NULL,               -- prompt, step, call, outcome and so on
        key  bytea,                              -- a prompt's key, as UTF-8
        item json        NOT NULL,               -- the item, exactly as written
        PRIMARY KEY (mind, at)
    );
    CREATE INDEX summaries ON items (mind, at) WHERE kind = 'summary';
    CREATE INDEX knowing ON items (mind, at)
        WHERE kind IN ('summary', 'attached', 'cloned', 'released', 'forked', 'parted',
                       'branched', 'ended');
    CREATE INDEX keys ON items (mind, key) WHERE key IS NOT NULL;
    CREATE TABLE hold (
        taken bigint NOT NULL -- one more each time a host takes the schema
    );
    INSERT INTO hold VALUES (0);
";

/// The tables in the schema a store is in.
const TABLES: &str = "SELECT table_name::text FROM information_schema.tables \
     WHERE table_schema = current_schema()";

/// A mind and its ancestors, youngest first: each one's name, whether it
/// is a root, and how many items it shares with its parent.
const LINEAGE: &str = "
    WITH RECURSIVE up (name, parent, inherited, depth) AS (
        SELECT name, parent, inherited, 0 FROM minds WHERE name = $1
        UNION ALL
        SELECT minds.name, minds.parent, minds.inherited, up.depth + 1
        FROM minds JOIN up ON minds.name = up.parent
    )
    SELECT name, parent IS NULL, inherited FROM up ORDER BY depth";

/// The rows of a lineage, given as each mind with the indices that are its
/// own in the history read: `$1` the minds, `$2` and `$3` the bounds.
macro_rules! lineage_rows {
    ($select:literal, $filter:literal) => {
        concat!(
            "SELECT ",
            $select,
            " FROM items JOIN unnest($1::text[], $2::bigint[], $3::bigint[]) \
             AS lineage (mind, low, high) \
             ON items.mind = lineage.mind AND items.at >= lineage.low AND items.at < lineage.high \
             WHERE ",
            $filter
        )
    };
}

const READ: &str = lineage_rows!(
    "items.item::text",
    "items.at >= $4 ORDER BY items.at LIMIT $5"
);
/// The last item of a lineage that `$filter` holds for. Each mind of the
/// lineage is searched back from its end, which stops at the first match,
/// and the youngest of those is the one: a join over the whole lineage
/// would read every match to keep one.
macro_rules! lineage_last {
    ($filter:literal) => {
        concat!(
            "SELECT last.item FROM unnest($1::text[], $2::bigint[], $3::bigint[]) \
             AS lineage (mind, low, high) CROSS JOIN LATERAL ( \
             SELECT items.at, items.item::text AS item FROM items \
             WHERE items.mind = lineage.mind AND items.at >= lineage.low \
             AND items.at < lineage.high AND ",
            $filter,
            " ORDER BY items.at DESC LIMIT 1) AS last ORDER BY last.at DESC LIMIT 1"
        )
    };
}

const LAST: &str = lineage_last!("items.kind = $4");
/// The last summary, which the index of them finds however long the history.
const LAST_SUMMARY: &str = lineage_last!("items.kind = 'summary'");
/// A keyed prompt, which the index of keys finds: searched back from each
/// mind's end it would read every item, as a new key matches none.
const KEYED: &str = lineage_rows!(
    "items.item::text",
    "items.key = $4 ORDER BY items.at DESC LIMIT 1"
);
const KNOWN: &str = lineage_rows!(
    "items.at, items.item::text",
    "items.at >= $4 \
     AND items.kind IN ('summary', 'attached', 'cloned', 'released', 'forked', 'parted', \
     'branched', 'ended') ORDER BY items.at"
);

const SETTINGS: &str = "SELECT digest, settings::text FROM settings";
const MINDS: &str = "SELECT record::text, settings FROM minds ORDER BY made";
const LIVE: &str = "SELECT name FROM minds WHERE NOT EXISTS (SELECT 1 FROM items \
     WHERE items.mind = minds.name AND items.kind = 'ended') ORDER BY made";
const RECORD: &str = "SELECT minds.record::text, settings.settings::text FROM minds \
     JOIN settings ON settings.digest = minds.settings WHERE minds.name = $1";
/// Whether the minds in the schema have records: an older mensor's had none.
const RECORDS: &str = "SELECT count(*) FROM information_schema.columns \
     WHERE table_schema = current_schema() AND table_name = 'minds' AND column_name = 'record'";
/// How long each of the minds `$1` is: each one's name, the items it
/// shares, and the index of its last own item.
const LENGTHS: &str = "SELECT minds.name, minds.inherited, \
     (SELECT max(at) FROM items WHERE items.mind = minds.name) \
     FROM minds WHERE minds.name = ANY($1)";
const INSERT_SETTINGS: &str =
    "INSERT INTO settings (digest, settings) VALUES ($1, $2::text::json) ON CONFLICT DO NOTHING";
const INSERT_MIND: &str = "INSERT INTO minds (name, parent, inherited, record, settings) \
     VALUES ($1, $2, $3, $4::text::json, $5)";
const INSERT_ITEMS: &str = "INSERT INTO items (mind, at, kind, key, item) \
     SELECT mind, at, kind, key, item::json \
     FROM unnest($1::text[], $2::bigint[], $3::text[], $4::bytea[], $5::text[]) \
     AS rows (mind, at, kind, key, item)";

/// A [`Store`] in one Postgres database.
pub struct PostgresStore {
    writer: Writer,
    reader: Reader,
}

impl std::fmt::Debug for PostgresStore {
    /// Nothing of the database: its URL holds a password.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresStore").finish_non_exhaustive()
    }
}

/// One item as its row holds it.
pub(crate) struct Row {
    kind: &'static str,
    key: Option<Vec<u8>>,
    item: String,
}

impl PostgresStore {
    /// Opens the database `url` names, and takes it for this host: makes its
    /// tables if its schema has none, and refuses one that holds others, or
    /// that another host holds. See the crate's docs for the URL.
    pub fn open(url: &str) -> Result<Self, StoreError> {
        let target = Arc::new(Target::parse(url)?);
        let writer = Writer::start(Session::new(Arc::clone(&target), true), commit);
        writer.run(prepare)?;
        let reader = Reader::start(Session::new(target, false));
        reader.run(|session| session.client().map(|_| ()))?;
        Ok(Self { writer, reader })
    }
}

/// Makes the tables in a schema with none, and takes the schema. A schema
/// that holds other tables, or another mensor's, is refused: its rows are
/// not these. So is a database whose encoding is not UTF-8: one item it
/// cannot hold would fail every append committed with it.
fn prepare(session: &mut Session) -> Result<(), StoreError> {
    let encoding: String = session
        .with(|client| client.query_one("SHOW server_encoding", &[]))?
        .get(0);
    if encoding != "UTF8" {
        return Err(StoreError::Backend(format!(
            "the Postgres database's encoding is {encoding}: the store needs UTF8"
        )));
    }
    let tables: Vec<String> = session
        .with(|client| client.query(TABLES, &[]))?
        .iter()
        .map(|row| row.get(0))
        .collect();
    let ours = ["hold", "items", "minds", "settings"];
    let mut sorted: Vec<&str> = tables.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    if sorted.is_empty() {
        session.with(|client| {
            let mut transaction = client.transaction()?;
            transaction.batch_execute(SCHEMA)?;
            transaction.commit()
        })?;
    } else {
        let records: i64 = session
            .with(|client| client.query_one(RECORDS, &[]))?
            .get(0);
        if sorted != ours || records != 1 {
            return Err(StoreError::Backend(
                "the Postgres schema holds tables this store does not: give it a database, or a \
                 schema, of its own"
                    .into(),
            ));
        }
    }
    session.take()
}

/// Commits a batch of appends as one, and says how each ended. An append to
/// a mind the store does not hold is refused by itself. If the database
/// fails, nothing of the batch is stored, and every append in it is told so.
///
/// A batch that found its connection lost before it asked to commit is
/// tried once more, on a new connection: nothing of it was stored. One lost
/// while it committed may have been stored, and is not.
fn commit(session: &mut Session, batch: &[Append]) -> Vec<Result<(), StoreError>> {
    let mut committing = false;
    let mut stored = store_batch(session, batch, &mut committing);
    if stored.is_err() && !committing && !session.connected() {
        stored = store_batch(session, batch, &mut committing);
    }
    stored.unwrap_or_else(|failed| {
        let failed = failed.to_string();
        let told = |_: &Append| Err(StoreError::Backend(failed.clone()));
        batch.iter().map(told).collect()
    })
}

fn store_batch(
    session: &mut Session,
    batch: &[Append],
    committing: &mut bool,
) -> Result<Vec<Result<(), StoreError>>, StoreError> {
    let lengths = session.statement(LENGTHS)?;
    let insert = session.statement(INSERT_ITEMS)?;
    let names: Vec<&str> = batch.iter().map(|append| append.name.as_str()).collect();
    session.with(|client| {
        let mut transaction = client.transaction()?;
        let mut next: HashMap<String, i64> = HashMap::new();
        for row in transaction.query(&lengths, &[&names])? {
            let last: Option<i64> = row.get(2);
            next.insert(row.get(0), last.map_or_else(|| row.get(1), |last| last + 1));
        }
        let mut ended = Vec::with_capacity(batch.len());
        let mut rows = Rows::default();
        for append in batch {
            match next.get_mut(append.name.as_str()) {
                Some(at) => {
                    for row in &append.rows {
                        rows.push(append.name.as_str(), *at, row);
                        *at += 1;
                    }
                    ended.push(Ok(()));
                }
                None => ended.push(Err(StoreError::Missing(append.name.clone()))),
            }
        }
        rows.insert(&mut transaction, &insert)?;
        *committing = true;
        transaction.commit()?;
        Ok(ended)
    })
}

/// Rows to insert, as the arrays `INSERT_ITEMS` takes.
#[derive(Default)]
struct Rows<'a> {
    minds: Vec<&'a str>,
    ats: Vec<i64>,
    kinds: Vec<&'static str>,
    keys: Vec<Option<&'a [u8]>>,
    items: Vec<&'a str>,
}

impl<'a> Rows<'a> {
    fn push(&mut self, mind: &'a str, at: i64, row: &'a Row) {
        self.minds.push(mind);
        self.ats.push(at);
        self.kinds.push(row.kind);
        self.keys.push(row.key.as_deref());
        self.items.push(&row.item);
    }

    fn insert(
        &self,
        transaction: &mut postgres::Transaction<'_>,
        insert: &postgres::Statement,
    ) -> Result<(), postgres::Error> {
        if !self.minds.is_empty() {
            let rows: [&(dyn postgres::types::ToSql + Sync); 5] =
                [&self.minds, &self.ats, &self.kinds, &self.keys, &self.items];
            transaction.execute(insert, &rows)?;
        }
        Ok(())
    }
}

/// Starts a mind's history, in one transaction. Run again after it was
/// stored, it finds the mind made, with the same record: so it may be.
fn create(session: &mut Session, mind: &MindRecord, own: &[Row]) -> Result<(), StoreError> {
    let record = session.statement(RECORD)?;
    let lengths = session.statement(LENGTHS)?;
    let insert_settings = session.statement(INSERT_SETTINGS)?;
    let insert_mind = session.statement(INSERT_MIND)?;
    let insert = session.statement(INSERT_ITEMS)?;
    let digest = mind.settings.digest();
    let settings = serde_json::to_string(&mind.settings).map_err(backend)?;
    let bare = MindRecord {
        settings: Arc::default(),
        ..mind.clone()
    };
    let written = serde_json::to_string(&bare).map_err(backend)?;
    session.with(|client| {
        let mut transaction = client.transaction()?;
        let found = transaction.query_opt(&record, &[&mind.name.as_str()])?;
        if let Some(found) = found {
            return Ok(match stored(found.get(0), found.get(1)) {
                Ok(found) if found == *mind => Ok(()),
                Ok(_) => Err(StoreError::Exists(mind.name.clone())),
                Err(error) => Err(error),
            });
        }
        let (parent, inherited) = match mind.origin.shares() {
            None => (None, 0),
            Some((parent, inherited)) => {
                let length = transaction.query_opt(&lengths, &[&vec![parent.as_str()]])?;
                let Some(length) = length else {
                    return Ok(Err(StoreError::Missing(parent.clone())));
                };
                let last: Option<i64> = length.get(2);
                let length = last.map_or_else(|| length.get::<_, i64>(1), |last| last + 1);
                let inherited = i64::try_from(inherited).unwrap_or(i64::MAX);
                if length < inherited {
                    return Ok(Err(StoreError::Short {
                        parent: parent.clone(),
                        inherited: unsigned(inherited),
                    }));
                }
                (Some(parent.as_str()), inherited)
            }
        };
        transaction.execute(&insert_settings, &[&digest, &settings])?;
        let row: [&(dyn postgres::types::ToSql + Sync); 5] =
            [&mind.name.as_str(), &parent, &inherited, &written, &digest];
        transaction.execute(&insert_mind, &row)?;
        let mut rows = Rows::default();
        for (at, row) in (inherited..).zip(own) {
            rows.push(mind.name.as_str(), at, row);
        }
        rows.insert(&mut transaction, &insert)?;
        transaction.commit()?;
        Ok(Ok(()))
    })?
}

/// The lineage of a mind, youngest first: the minds whose rows hold its
/// history, and the indices that are each one's own, as `$1` to `$3` of the
/// queries that read through it.
struct Lineage {
    minds: Vec<String>,
    lows: Vec<i64>,
    highs: Vec<i64>,
}

impl Lineage {
    fn of(session: &mut Session, name: &Name) -> Result<Self, StoreError> {
        let statement = session.statement(LINEAGE)?;
        let rows = session.with(|client| client.query(&statement, &[&name.as_str()]))?;
        if rows.is_empty() {
            return Err(StoreError::Missing(name.clone()));
        }
        let mut lineage = Self {
            minds: Vec::new(),
            lows: Vec::new(),
            highs: Vec::new(),
        };
        let mut end = i64::MAX;
        for row in rows {
            let (mind, root, inherited): (String, bool, i64) = (row.get(0), row.get(1), row.get(2));
            lineage.minds.push(mind);
            lineage.highs.push(end);
            if root {
                lineage.lows.push(0);
            } else {
                lineage.lows.push(inherited.min(end));
                end = end.min(inherited);
            }
        }
        Ok(lineage)
    }

    /// The lineage cut at index `before`: only the indices below it.
    fn before(mut self, before: u64) -> Self {
        let before = saturated(before);
        for high in &mut self.highs {
            *high = (*high).min(before);
        }
        self
    }

    /// The items of the lineage that `sql` selects, with `then` as its
    /// parameters after the lineage's.
    fn items(
        &self,
        session: &mut Session,
        sql: &'static str,
        then: &[&(dyn postgres::types::ToSql + Sync)],
    ) -> Result<Vec<postgres::Row>, StoreError> {
        let statement = session.statement(sql)?;
        let mut parameters: Vec<&(dyn postgres::types::ToSql + Sync)> =
            vec![&self.minds, &self.lows, &self.highs];
        parameters.extend_from_slice(then);
        session.with(|client| client.query(&statement, &parameters))
    }

    /// The last item `sql` selects, with `then` as its parameters after
    /// the lineage's.
    fn last(
        &self,
        session: &mut Session,
        sql: &'static str,
        then: &[&(dyn postgres::types::ToSql + Sync)],
    ) -> Result<Option<Item>, StoreError> {
        let rows = self.items(session, sql, then)?;
        rows.first().map(|row| parse(row.get(0))).transpose()
    }

    /// The items from `from`, `limit` of them at the most.
    fn read(
        &self,
        session: &mut Session,
        from: u64,
        limit: usize,
    ) -> Result<Vec<Item>, StoreError> {
        let (from, limit) = (saturated(from), i64::try_from(limit).unwrap_or(i64::MAX));
        let rows = self.items(session, READ, &[&from, &limit])?;
        rows.iter().map(|row| parse(row.get(0))).collect()
    }

    /// The index the last summary starts from, or 0.
    fn summary_from(&self, session: &mut Session) -> Result<u64, StoreError> {
        Ok(match self.last(session, LAST_SUMMARY, &[])? {
            Some(Item::Summary { from, .. }) => from,
            _ => 0,
        })
    }

    /// The items from `from` that say what the mind knows of its hands and
    /// kin, each with its index.
    fn known(&self, session: &mut Session, from: u64) -> Result<Vec<(u64, Item)>, StoreError> {
        let rows = self.items(session, KNOWN, &[&saturated(from)])?;
        rows.iter()
            .map(|row| Ok((unsigned(row.get(0)), parse(row.get(1))?)))
            .collect()
    }
}

impl Store for PostgresStore {
    fn create(&self, mind: &MindRecord, items: &[Item]) -> Result<(), StoreError> {
        let (mind, own) = (mind.clone(), rows(items)?);
        self.writer.run(move |session| create(session, &mind, &own))
    }

    fn append(&self, name: &Name, items: &[Item]) -> Result<(), StoreError> {
        self.writer.append(name, rows(items)?)
    }

    fn read(&self, name: &Name, from: u64, limit: usize) -> Result<Vec<Item>, StoreError> {
        let name = name.clone();
        self.reader
            .run(move |session| Lineage::of(session, &name)?.read(session, from, limit))
    }

    fn last(&self, name: &Name, kind: &str, before: u64) -> Result<Option<Item>, StoreError> {
        let (name, kind) = (name.clone(), kind.to_owned());
        self.reader.run(move |session| {
            let lineage = Lineage::of(session, &name)?.before(before);
            if kind == "summary" {
                return lineage.last(session, LAST_SUMMARY, &[]);
            }
            lineage.last(session, LAST, &[&kind])
        })
    }

    fn tail(&self, name: &Name) -> Result<(u64, Vec<Item>), StoreError> {
        let name = name.clone();
        self.reader.run(move |session| {
            let lineage = Lineage::of(session, &name)?;
            let from = lineage.summary_from(session)?;
            Ok((from, lineage.read(session, from, usize::MAX)?))
        })
    }

    fn recent(&self, name: &Name) -> Result<Latest, StoreError> {
        let name = name.clone();
        self.reader.run(move |session| {
            let lineage = Lineage::of(session, &name)?;
            let from = match lineage.last(session, LAST, &[&"step"])? {
                Some(Item::Step { seen, .. }) => seen,
                _ => 0,
            };
            let summary = lineage.summary_from(session)?;
            let known = lineage.known(session, summary)?;
            Ok(Latest {
                known: known.into_iter().filter(|(at, _)| *at < from).collect(),
                from,
                items: lineage.read(session, from, usize::MAX)?,
            })
        })
    }

    fn known(&self, name: &Name) -> Result<Vec<(u64, Item)>, StoreError> {
        let name = name.clone();
        self.reader.run(move |session| {
            let lineage = Lineage::of(session, &name)?;
            let from = lineage.summary_from(session)?;
            lineage.known(session, from)
        })
    }

    fn prompt(&self, name: &Name, key: &str) -> Result<Option<String>, StoreError> {
        let (name, key) = (name.clone(), key.as_bytes().to_vec());
        self.reader.run(move |session| {
            let lineage = Lineage::of(session, &name)?;
            Ok(match lineage.last(session, KEYED, &[&key])? {
                Some(Item::Prompt { text, .. }) => Some(text),
                _ => None,
            })
        })
    }

    fn live(&self) -> Result<Vec<Name>, StoreError> {
        self.reader.run(|session| {
            let statement = session.statement(LIVE)?;
            let rows = session.with(|client| client.query(&statement, &[]))?;
            rows.iter()
                .map(|row| Name::new(row.get::<_, &str>(0)).map_err(backend))
                .collect()
        })
    }

    fn minds(&self) -> Result<Vec<MindRecord>, StoreError> {
        self.reader.run(|session| {
            // Each settings once, shared by every mind made with them.
            let statement = session.statement(SETTINGS)?;
            let rows = session.with(|client| client.query(&statement, &[]))?;
            let mut shared = HashMap::new();
            for row in rows {
                let settings: MindSettings = serde_json::from_str(row.get(1)).map_err(backend)?;
                shared.insert(row.get::<_, String>(0), Arc::new(settings));
            }
            let statement = session.statement(MINDS)?;
            let rows = session.with(|client| client.query(&statement, &[]))?;
            rows.iter()
                .map(|row| {
                    let settings = shared.get(row.get::<_, &str>(1)).cloned();
                    let settings =
                        settings.ok_or_else(|| backend("a mind's settings are missing"))?;
                    let record: MindRecord = serde_json::from_str(row.get(0)).map_err(backend)?;
                    Ok(MindRecord { settings, ..record })
                })
                .collect()
        })
    }
}

/// A record as stored, with the settings it was stored apart from.
fn stored(record: &str, settings: &str) -> Result<MindRecord, StoreError> {
    let record: MindRecord = serde_json::from_str(record).map_err(backend)?;
    let settings: MindSettings = serde_json::from_str(settings).map_err(backend)?;
    Ok(MindRecord {
        settings: Arc::new(settings),
        ..record
    })
}

/// The rows `items` are stored as.
fn rows(items: &[Item]) -> Result<Vec<Row>, StoreError> {
    items
        .iter()
        .map(|item| {
            Ok(Row {
                kind: item.kind(),
                key: item.key().map(|key| key.as_bytes().to_vec()),
                item: serde_json::to_string(item).map_err(backend)?,
            })
        })
        .collect()
}

fn parse(item: &str) -> Result<Item, StoreError> {
    serde_json::from_str(item).map_err(backend)
}

/// An index as Postgres holds it: one past `i64::MAX` is past every row.
fn saturated(index: u64) -> i64 {
    i64::try_from(index).unwrap_or(i64::MAX)
}

/// An index Postgres held, which is never negative.
fn unsigned(index: i64) -> u64 {
    u64::try_from(index).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use mensor_mind::KNOWING;

    use super::*;

    /// The kinds an SQL statement lists in its first `IN (...)`.
    fn kinds(sql: &str) -> Vec<&str> {
        let listed = sql.split("IN (").nth(1).unwrap().split(')').next().unwrap();
        listed
            .split(',')
            .map(|kind| kind.trim().trim_matches('\''))
            .collect()
    }

    #[test]
    fn the_kinds_read_as_known_are_those_a_mind_knows_by() {
        assert_eq!(kinds(KNOWN), KNOWING);
        assert_eq!(kinds(SCHEMA), KNOWING);
    }
}

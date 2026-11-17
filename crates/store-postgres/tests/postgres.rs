//! The Postgres store against a real Postgres, with TLS: `make test-postgres`
//! starts one in Docker, names it in `MENSOR_TEST_POSTGRES` and the root
//! that signs its certificate in `MENSOR_TEST_POSTGRES_ROOT`, and runs these.
//! Each test makes a database of its own.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use mensor_mind::{Item, MindRecord, Origin, Store, StoreError};
use mensor_protocol::Name;
use mensor_store_postgres::PostgresStore;
use postgres::{Client, NoTls};

mensor_mind::store_suite!(fresh(), #[ignore = "needs Postgres: make test-postgres"]);

/// The server, as `postgres://user:password@host:port`.
fn server() -> String {
    std::env::var("MENSOR_TEST_POSTGRES")
        .expect("make test-postgres names a Postgres in MENSOR_TEST_POSTGRES")
}

/// A connection of the test's own, plain, to `database`.
fn connect(database: &str) -> Client {
    Client::connect(&plain(database), NoTls).unwrap()
}

/// The name of a new, empty database.
fn database() -> String {
    static MADE: AtomicU32 = AtomicU32::new(0);
    let name = format!(
        "mensor_{}_{}",
        std::process::id(),
        MADE.fetch_add(1, Ordering::SeqCst)
    );
    connect("postgres")
        .batch_execute(&format!("CREATE DATABASE {name}"))
        .unwrap();
    name
}

/// A URL for `database` that asks for no TLS.
fn plain(database: &str) -> String {
    format!("{}/{database}?sslmode=disable", server())
}

/// A store over a new, empty database.
fn fresh() -> PostgresStore {
    PostgresStore::open(&plain(&database())).unwrap()
}

fn name(name: &str) -> Name {
    Name::new(name).unwrap()
}

fn prompt(text: &str) -> Item {
    Item::Prompt {
        key: None,
        text: text.into(),
        from: None,
    }
}

fn root(store: &PostgresStore, mind: &str, items: &[Item]) {
    let record = MindRecord {
        name: name(mind),
        origin: Origin::Root,
    };
    store.create(&record, &[]).unwrap();
    store.append(&name(mind), items).unwrap();
}

fn texts(store: &PostgresStore, mind: &str) -> Vec<String> {
    let items = store.read(&name(mind), 0, usize::MAX).unwrap();
    let text = |item: Item| match item {
        Item::Prompt { text, .. } => text,
        other => panic!("not a prompt: {other:?}"),
    };
    items.into_iter().map(text).collect()
}

/// Ends every connection to `database` but the test's own, and waits until
/// they are gone.
fn cut(database: &str) {
    let mut admin = connect("postgres");
    let others = "FROM pg_stat_activity WHERE datname = $1 AND pid <> pg_backend_pid()";
    admin
        .execute(
            &format!("SELECT pg_terminate_backend(pid) {others}"),
            &[&database],
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left: i64 = admin
            .query_one(&format!("SELECT count(*) {others}"), &[&database])
            .unwrap()
            .get(0);
        if left == 0 {
            return;
        }
        assert!(Instant::now() < deadline, "the cut connections did not end");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
#[ignore = "needs Postgres: make test-postgres"]
fn a_fork_stores_only_its_own_rows() {
    let database = database();
    let store = PostgresStore::open(&plain(&database)).unwrap();
    root(&store, "a1", &[prompt("0"), prompt("1"), prompt("2")]);
    let child = MindRecord {
        name: name("b7"),
        origin: Origin::Fork {
            parent: name("a1"),
            inherited: 2,
        },
    };
    store.create(&child, &[prompt("b2")]).unwrap();
    let own: i64 = connect(&database)
        .query_one("SELECT count(*) FROM items WHERE mind = 'b7'", &[])
        .unwrap()
        .get(0);
    assert_eq!(own, 1);
    assert_eq!(store.read(&name("b7"), 0, usize::MAX).unwrap().len(), 3);
}

#[test]
#[ignore = "needs Postgres: make test-postgres"]
fn a_row_says_when_it_was_stored_what_it_is_and_its_key() {
    let database = database();
    let store = PostgresStore::open(&plain(&database)).unwrap();
    let keyed = Item::Prompt {
        key: Some("k é".into()),
        text: "hello".into(),
        from: None,
    };
    root(&store, "a1", std::slice::from_ref(&keyed));
    let row = connect(&database)
        .query_one(
            "SELECT now() - time < interval '1 minute', kind, convert_from(key, 'UTF8'), \
             item::text FROM items",
            &[],
        )
        .unwrap();
    assert!(row.get::<_, bool>(0));
    assert_eq!(row.get::<_, &str>(1), "prompt");
    assert_eq!(row.get::<_, &str>(2), "k é");
    // The item as it was written, to the byte, and readable in SQL.
    assert_eq!(
        row.get::<_, &str>(3),
        serde_json::to_string(&keyed).unwrap()
    );
    let text: String = connect(&database)
        .query_one("SELECT item ->> 'text' FROM items", &[])
        .unwrap()
        .get(0);
    assert_eq!(text, "hello");
}

#[test]
#[ignore = "needs Postgres: make test-postgres"]
fn a_schema_that_holds_other_tables_is_refused() {
    let database = database();
    connect(&database)
        .batch_execute("CREATE TABLE notes (text text)")
        .unwrap();
    let refused = PostgresStore::open(&plain(&database)).unwrap_err();
    assert!(refused.to_string().contains("of its own"), "{refused}");
}

#[test]
#[ignore = "needs Postgres: make test-postgres"]
fn a_database_not_in_utf8_is_refused() {
    let name = format!("{}_latin1", database());
    connect("postgres")
        .batch_execute(&format!(
            "CREATE DATABASE {name} ENCODING 'LATIN1' LC_COLLATE 'C' LC_CTYPE 'C' \
             TEMPLATE template0"
        ))
        .unwrap();
    let refused = PostgresStore::open(&plain(&name)).unwrap_err();
    assert!(
        refused.to_string().contains("encoding is LATIN1"),
        "{refused}"
    );
}

#[test]
#[ignore = "needs Postgres: make test-postgres"]
fn a_schema_of_its_own_in_a_shared_database_is_used() {
    let database = database();
    connect(&database)
        .batch_execute("CREATE TABLE notes (text text); CREATE SCHEMA mensor")
        .unwrap();
    let url = format!("{}&options=-c%20search_path%3Dmensor", plain(&database));
    let store = PostgresStore::open(&url).unwrap();
    root(&store, "a1", &[prompt("0")]);
    let schema: String = connect(&database)
        .query_one(
            "SELECT table_schema::text FROM information_schema.tables WHERE table_name = 'items'",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(schema, "mensor");
}

#[test]
#[ignore = "needs Postgres: make test-postgres"]
fn everything_is_there_when_the_database_is_opened_again() {
    let url = plain(&database());
    {
        let store = PostgresStore::open(&url).unwrap();
        root(&store, "a1", &[prompt("0"), prompt("1")]);
    }
    let store = PostgresStore::open(&url).unwrap();
    assert_eq!(store.minds().unwrap().len(), 1);
    store.append(&name("a1"), &[prompt("2")]).unwrap();
    assert_eq!(store.read(&name("a1"), 1, usize::MAX).unwrap().len(), 2);
}

#[test]
#[ignore = "needs Postgres: make test-postgres"]
fn a_second_host_is_refused_until_the_first_has_gone() {
    let url = plain(&database());
    let first = PostgresStore::open(&url).unwrap();
    let refused = PostgresStore::open(&url).unwrap_err();
    assert!(
        refused.to_string().contains("another mensor host"),
        "{refused}"
    );
    drop(first);
    PostgresStore::open(&url).unwrap();
}

#[test]
#[ignore = "needs Postgres: make test-postgres"]
fn a_store_carries_on_when_its_connections_are_cut() {
    let database = database();
    let url = plain(&database);
    let store = PostgresStore::open(&url).unwrap();
    root(&store, "a1", &[prompt("0")]);
    cut(&database);

    // Each connects again, what found its connection gone runs again, and
    // only the writer takes the database back: a read first takes nothing.
    assert_eq!(texts(&store, "a1"), ["0"]);
    store.append(&name("a1"), &[prompt("1")]).unwrap();
    assert_eq!(texts(&store, "a1"), ["0", "1"]);
    let refused = PostgresStore::open(&url).unwrap_err();
    assert!(
        refused.to_string().contains("another mensor host"),
        "{refused}"
    );
}

#[test]
#[ignore = "needs Postgres: make test-postgres"]
fn a_host_whose_database_another_took_while_it_was_away_stores_nothing_more() {
    let database = database();
    let url = plain(&database);
    let away = PostgresStore::open(&url).unwrap();
    root(&away, "a1", &[prompt("0")]);
    cut(&database);
    {
        let other = PostgresStore::open(&url).unwrap();
        other.append(&name("a1"), &[prompt("other")]).unwrap();
    }

    // What the first host holds in memory is out of date: it may read, and
    // write nothing, however often it tries.
    for _ in 0..2 {
        let refused = away.append(&name("a1"), &[prompt("away")]).unwrap_err();
        assert!(refused.to_string().contains("start it again"), "{refused}");
    }
    let made = MindRecord {
        name: name("b2"),
        origin: Origin::Root,
    };
    assert!(away.create(&made, &[]).is_err());
    assert_eq!(texts(&away, "a1"), ["0", "other"]);

    // Started again, it carries on from what is stored.
    drop(away);
    let again = PostgresStore::open(&url).unwrap();
    again.append(&name("a1"), &[prompt("again")]).unwrap();
    assert_eq!(texts(&again, "a1"), ["0", "other", "again"]);
}

/// A trigger function that ends its own connection the first time it runs,
/// and lets every other run by.
const KILL_ONCE: &str = "
    CREATE SEQUENCE runs;
    CREATE FUNCTION kill_once() RETURNS trigger LANGUAGE plpgsql AS $$
    BEGIN
        IF nextval('runs') = 1 THEN
            PERFORM pg_terminate_backend(pg_backend_pid());
            PERFORM pg_sleep(5);
        END IF;
        RETURN NEW;
    END $$;
";

#[test]
#[ignore = "needs Postgres: make test-postgres"]
fn a_batch_lost_before_it_commits_is_stored_once_on_a_new_connection() {
    let database = database();
    let store = PostgresStore::open(&plain(&database)).unwrap();
    root(&store, "a1", &[]);
    connect(&database)
        .batch_execute(&format!(
            "{KILL_ONCE} CREATE TRIGGER kill BEFORE INSERT ON items \
             FOR EACH ROW EXECUTE FUNCTION kill_once()"
        ))
        .unwrap();
    store.append(&name("a1"), &[prompt("once")]).unwrap();
    assert_eq!(texts(&store, "a1"), ["once"]);
}

#[test]
#[ignore = "needs Postgres: make test-postgres"]
fn a_batch_lost_as_it_commits_fails_and_is_not_tried_again() {
    let database = database();
    let store = PostgresStore::open(&plain(&database)).unwrap();
    root(&store, "a1", &[]);
    // Deferred, so it runs as the batch commits.
    connect(&database)
        .batch_execute(&format!(
            "{KILL_ONCE} CREATE CONSTRAINT TRIGGER kill AFTER INSERT ON items \
             DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION kill_once()"
        ))
        .unwrap();
    // A second try would have been stored, the trigger letting it by: it
    // was not made.
    assert!(store.append(&name("a1"), &[prompt("lost")]).is_err());
    assert!(texts(&store, "a1").is_empty());
    store.append(&name("a1"), &[prompt("after")]).unwrap();
    assert_eq!(texts(&store, "a1"), ["after"]);
}

#[test]
#[ignore = "needs Postgres: make test-postgres"]
fn a_read_waits_for_no_commit() {
    let database = database();
    let store = Arc::new(PostgresStore::open(&plain(&database)).unwrap());
    root(&store, "a1", &[prompt("0")]);
    // Another session keeps the store from writing, as a slow commit would.
    let mut holder = connect(&database);
    let mut holding = holder.transaction().unwrap();
    holding
        .batch_execute("LOCK TABLE items IN EXCLUSIVE MODE")
        .unwrap();
    let in_background = |work: fn(&PostgresStore) -> usize| {
        let (store, (done, finished)) = (Arc::clone(&store), mpsc::channel());
        std::thread::spawn(move || done.send(work(&store)));
        finished
    };
    let appending = in_background(|store| {
        store.append(&name("a1"), &[prompt("1")]).unwrap();
        1
    });
    let reading = in_background(|store| store.read(&name("a1"), 0, usize::MAX).unwrap().len());
    assert_eq!(reading.recv_timeout(Duration::from_secs(10)), Ok(1));
    assert!(appending.recv_timeout(Duration::from_millis(200)).is_err());
    holding.rollback().unwrap();
    assert_eq!(appending.recv_timeout(Duration::from_secs(10)), Ok(1));
    assert_eq!(texts(&store, "a1"), ["0", "1"]);
}

/// A URL for `database` over TLS, checked against the test's root.
fn checked(database: &str) -> String {
    let root = std::env::var("MENSOR_TEST_POSTGRES_ROOT")
        .expect("make test-postgres names its root in MENSOR_TEST_POSTGRES_ROOT");
    format!("{}/{database}?sslrootcert={root}", server())
}

#[test]
#[ignore = "needs Postgres: make test-postgres"]
fn a_url_that_names_a_root_reaches_postgres_over_tls_checked_by_it() {
    let database = database();
    let store = PostgresStore::open(&checked(&database)).unwrap();
    root(&store, "a1", &[prompt("0")]);
    let tls: Vec<bool> = connect("postgres")
        .query(
            "SELECT ssl FROM pg_stat_ssl JOIN pg_stat_activity USING (pid) WHERE datname = $1",
            &[&database],
        )
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(tls, [true, true], "both of the store's connections");
    let shown = format!("{store:?}");
    assert!(!shown.contains("not-a-secret"), "{shown}");
}

#[test]
#[ignore = "needs Postgres: make test-postgres"]
fn a_server_no_known_root_vouches_for_is_refused() {
    // No sslrootcert: the system's roots, none of which signed the test's.
    let url = format!("{}/{}", server(), database());
    let refused = PostgresStore::open(&url).unwrap_err().to_string();
    assert!(
        refused.contains("invalid peer certificate: UnknownIssuer"),
        "{refused}"
    );
    assert!(!refused.contains("not-a-secret"), "{refused}");
}

#[test]
fn a_database_that_cannot_be_reached_is_refused_without_its_password() {
    let refused =
        PostgresStore::open("postgres://mensor:not-a-secret@127.0.0.1:1/m?sslmode=disable")
            .unwrap_err();
    assert!(matches!(refused, StoreError::Backend(_)), "{refused:?}");
    assert!(!refused.to_string().contains("not-a-secret"), "{refused}");
}

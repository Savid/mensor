//! What every [`Store`] does, as tests any store runs over itself.
//!
//! Each function takes a new, empty store and panics where it differs from
//! what a store must do. [`store_suite!`](crate::store_suite) writes a test
//! for each, so a store's crate runs them all with one line.

use std::collections::BTreeMap;

use mensor_protocol::Name;
use serde_json::json;

use crate::{Item, MindRecord, Origin, Outcome, Stop, Store, StoreError, Usage};

/// Writes a `#[test]` for each function in [`suite`](crate::suite), over
/// the store `$make` makes anew for each. Attributes after it, such as
/// `#[ignore]`, go on every test.
#[macro_export]
macro_rules! store_suite {
    ($make:expr $(, #[$attribute:meta])* $(,)?) => {
        $crate::store_suite!(@each [$(#[$attribute])*] $make;
            a_history_reads_back_from_any_index,
            a_history_is_read_a_page_at_a_time_and_from_its_last_summary,
            a_fork_shares_its_parents_first_items_and_no_more,
            what_a_mind_knows_is_read_through_its_parent_from_its_last_summary,
            a_mind_made_again_with_its_origin_is_the_one_made_before,
            minds_are_listed_in_the_order_they_were_made,
            a_store_refuses_what_it_cannot_do,
            an_item_reads_back_as_it_was_stored,
            appends_that_arrive_together_are_each_stored_whole_and_in_order,
        );
    };
    (@each $attributes:tt $make:expr; $($test:ident),* $(,)?) => {
        $($crate::store_suite!(@one $attributes $make; $test);)*
    };
    (@one [$($attribute:tt)*] $make:expr; $test:ident) => {
        #[test]
        $($attribute)*
        fn $test() {
            $crate::suite::$test(&$make);
        }
    };
}

/// A history reads back whole, or from any index.
#[expect(
    clippy::missing_panics_doc,
    reason = "a test: panicking is how it fails"
)]
pub fn a_history_reads_back_from_any_index(store: &dyn Store) {
    root(store, "a1", &["0", "1", "2"]);
    store.append(&name("a1"), &[prompt("3")]).unwrap();
    assert_eq!(texts(&read(store, "a1", 0)), ["0", "1", "2", "3"]);
    assert_eq!(texts(&read(store, "a1", 2)), ["2", "3"]);
    assert!(read(store, "a1", 9).is_empty());
}

/// A history is read a page at a time, a mind is loaded from its last
/// summary, and a prompt is found by its key, through a parent's items.
#[expect(
    clippy::missing_panics_doc,
    reason = "a test: panicking is how it fails"
)]
pub fn a_history_is_read_a_page_at_a_time_and_from_its_last_summary(store: &dyn Store) {
    root(store, "a1", &["0", "1", "2"]);
    assert_eq!(texts(&store.read(&name("a1"), 1, 1).unwrap()), ["1"]);
    assert_eq!(store.tail(&name("a1")).unwrap().0, 0);

    let keyed = Item::Prompt {
        key: Some("k".into()),
        text: "3".into(),
        from: None,
    };
    store.append(&name("a1"), &[keyed, summary(2)]).unwrap();
    fork(store, "b7", "a1", 5);
    store.append(&name("b7"), &[prompt("5")]).unwrap();
    store.append(&name("a1"), &[summary(5)]).unwrap();

    // A fork's history may start in its parent's, and holds nothing its
    // parent stored after the two parted.
    let (from, items) = store.tail(&name("b7")).unwrap();
    assert_eq!((from, items.len()), (2, 4));
    assert_eq!(store.tail(&name("a1")).unwrap().0, 5);
    assert_eq!(
        store.prompt(&name("b7"), "k").unwrap().as_deref(),
        Some("3")
    );
    assert_eq!(store.prompt(&name("b7"), "other").unwrap(), None);
    // A page may span the two.
    assert_eq!(store.read(&name("b7"), 3, 2).unwrap().len(), 2);
    assert_eq!(store.read(&name("b7"), 4, 9).unwrap().len(), 2);
    // With no step in it, the latest of a history is all of it.
    assert_eq!(store.recent(&name("b7")).unwrap().from, 0);

    // The latest of a history starts at what its last step had seen.
    let step = Item::Step {
        seen: 6,
        content: Vec::new(),
        stop: Stop::EndTurn,
        usage: Usage::default(),
    };
    store.append(&name("b7"), &[prompt("6"), step]).unwrap();
    let latest = store.recent(&name("b7")).unwrap();
    assert_eq!((latest.from, latest.items.len()), (6, 2));
    assert_eq!(store.last(&name("b7"), "no such kind").unwrap(), None);
    assert!(matches!(
        store.last(&name("b7"), "summary").unwrap(),
        Some(Item::Summary { from: 2, .. })
    ));
}

/// A fork reads its parent's first items, and none its parent stores
/// after, through any number of forks.
#[expect(
    clippy::missing_panics_doc,
    reason = "a test: panicking is how it fails"
)]
pub fn a_fork_shares_its_parents_first_items_and_no_more(store: &dyn Store) {
    root(store, "a1", &["0", "1", "2"]);
    fork(store, "b7", "a1", 2);
    store
        .append(&name("b7"), &[prompt("b2"), prompt("b3")])
        .unwrap();
    store.append(&name("a1"), &[prompt("3")]).unwrap();
    fork(store, "c9", "b7", 3);
    store.append(&name("c9"), &[prompt("c3")]).unwrap();
    fork(store, "d4", "b7", 1);
    fork(store, "e5", "b7", 4);

    assert_eq!(texts(&read(store, "a1", 0)), ["0", "1", "2", "3"]);
    assert_eq!(texts(&read(store, "b7", 0)), ["0", "1", "b2", "b3"]);
    assert_eq!(texts(&read(store, "b7", 1)), ["1", "b2", "b3"]);
    assert_eq!(texts(&read(store, "c9", 0)), ["0", "1", "b2", "c3"]);
    assert_eq!(texts(&read(store, "c9", 3)), ["c3"]);
    assert_eq!(texts(&read(store, "d4", 0)), ["0"]);
    assert_eq!(texts(&read(store, "e5", 0)), ["0", "1", "b2", "b3"]);
}

/// What a mind knows of its hands and kin is read through its parent, from
/// its last summary.
#[expect(
    clippy::missing_panics_doc,
    reason = "a test: panicking is how it fails"
)]
pub fn what_a_mind_knows_is_read_through_its_parent_from_its_last_summary(store: &dyn Store) {
    let released = Item::Released {
        hand: name("b2"),
        reason: "gone".into(),
    };
    let parted = Item::Parted {
        parent: name("a1"),
        left: 31,
    };
    store.create(&record("a1", Origin::Root), &[]).unwrap();
    store
        .append(&name("a1"), &[prompt("0"), released.clone(), prompt("2")])
        .unwrap();
    let child = Origin::Fork {
        parent: name("a1"),
        inherited: 3,
    };
    store
        .create(&record("b7", child), &[parted.clone(), prompt("4")])
        .unwrap();
    assert_eq!(
        store.known(&name("b7")).unwrap(),
        [(1, released), (3, parted.clone())]
    );
    // A summary that starts from 2 leaves out what came before it.
    store.append(&name("b7"), &[summary(2)]).unwrap();
    assert_eq!(
        store.known(&name("b7")).unwrap(),
        [(3, parted), (5, summary(2))]
    );
    let latest = store.recent(&name("b7")).unwrap();
    assert_eq!((latest.from, latest.items.len()), (0, 6));
}

/// A mind made again with the origin it was made with is the one made
/// before, with the first items it was made with, and nothing is written.
#[expect(
    clippy::missing_panics_doc,
    reason = "a test: panicking is how it fails"
)]
pub fn a_mind_made_again_with_its_origin_is_the_one_made_before(store: &dyn Store) {
    root(store, "m", &["0", "1"]);
    let child = || {
        let origin = Origin::Fork {
            parent: name("m"),
            inherited: 2,
        };
        store.create(&record("m.d3342iuj", origin), &[prompt("c2"), prompt("c3")])
    };
    child().unwrap();
    child().unwrap();
    assert_eq!(texts(&read(store, "m.d3342iuj", 0)), ["0", "1", "c2", "c3"]);
    store.append(&name("m.d3342iuj"), &[prompt("c4")]).unwrap();
    assert_eq!(texts(&read(store, "m.d3342iuj", 4)), ["c4"]);
}

/// Minds are listed in the order they were made, each with its origin.
#[expect(
    clippy::missing_panics_doc,
    reason = "a test: panicking is how it fails"
)]
pub fn minds_are_listed_in_the_order_they_were_made(store: &dyn Store) {
    root(store, "z9", &["0"]);
    fork(store, "a1", "z9", 1);
    root(store, "b2", &[]);
    let forked = Origin::Fork {
        parent: name("z9"),
        inherited: 1,
    };
    assert_eq!(
        store.minds().unwrap(),
        [
            record("z9", Origin::Root),
            record("a1", forked),
            record("b2", Origin::Root),
        ]
    );
}

/// A store refuses a second origin for a name, a mind it does not hold, and
/// a fork of more items than its parent has.
#[expect(
    clippy::missing_panics_doc,
    reason = "a test: panicking is how it fails"
)]
pub fn a_store_refuses_what_it_cannot_do(store: &dyn Store) {
    root(store, "a1", &["0"]);
    let elsewhere = Origin::Fork {
        parent: name("a1"),
        inherited: 0,
    };
    let again = store.create(&record("a1", elsewhere), &[]);
    assert!(matches!(again, Err(StoreError::Exists(_))), "{again:?}");
    let append = store.append(&name("x"), &[prompt("0")]);
    assert!(matches!(append, Err(StoreError::Missing(_))), "{append:?}");
    let read = store.read(&name("x"), 0, usize::MAX);
    assert!(matches!(read, Err(StoreError::Missing(_))), "{read:?}");

    let orphan = Origin::Fork {
        parent: name("x"),
        inherited: 0,
    };
    let orphan = store.create(&record("b7", orphan), &[]);
    assert!(matches!(orphan, Err(StoreError::Missing(_))), "{orphan:?}");
    let long = Origin::Fork {
        parent: name("a1"),
        inherited: 2,
    };
    let long = store.create(&record("b7", long), &[]);
    assert!(matches!(long, Err(StoreError::Short { .. })), "{long:?}");
    assert_eq!(store.minds().unwrap().len(), 1);
}

/// An item reads back as it was stored, whatever its text holds: a NUL, a
/// line break, any script, a number of any size.
#[expect(
    clippy::missing_panics_doc,
    reason = "a test: panicking is how it fails"
)]
pub fn an_item_reads_back_as_it_was_stored(store: &dyn Store) {
    let held = Item::Prompt {
        key: Some("k\u{0}é".into()),
        text: "a\u{0}b\r\nc 漢字 🦀 \u{7f} \\ \"".into(),
        from: None,
    };
    let output = Item::Outcome {
        tool_use: "tu_1".into(),
        call: Some(7),
        outcome: Outcome::Output(json!({
            "stdout": "\u{0}\u{1}\u{1b}[0m",
            "exit": 0,
            "big": u64::MAX,
            "small": i64::MIN,
            "half": 0.5,
            "far": 1e300,
            "z": {"b": 1, "a": [null, true, "", {}]},
        })),
    };
    root(store, "a1", &[]);
    store
        .append(&name("a1"), &[held.clone(), output.clone()])
        .unwrap();
    assert_eq!(read(store, "a1", 0), [held.clone(), output.clone()]);
    assert_eq!(store.last(&name("a1"), "outcome").unwrap(), Some(output));
    let text = store.prompt(&name("a1"), "k\u{0}é").unwrap();
    assert_eq!(text.as_deref(), Some("a\u{0}b\r\nc 漢字 🦀 \u{7f} \\ \""));
}

/// Appends that arrive together, from many threads, are each stored whole
/// and in the order each thread made them. One to a mind the store does not
/// hold fails alone.
#[expect(
    clippy::missing_panics_doc,
    reason = "a test: panicking is how it fails"
)]
pub fn appends_that_arrive_together_are_each_stored_whole_and_in_order(store: &dyn Store) {
    let threads: Vec<String> = (0..16).map(|thread| format!("t{thread}")).collect();
    root(store, "all", &[]);
    for thread in &threads {
        root(store, thread, &[]);
    }
    std::thread::scope(|scope| {
        for thread in &threads {
            scope.spawn(move || {
                for index in 0..40 {
                    let said = [prompt(&format!("{thread} {index}")), prompt("and")];
                    store.append(&name(thread), &said).unwrap();
                    store.append(&name("all"), &said[..1]).unwrap();
                    let refused = store.append(&name("nobody"), &said);
                    assert!(matches!(refused, Err(StoreError::Missing(_))));
                }
            });
        }
    });

    for thread in &threads {
        let expected: Vec<String> = (0..40)
            .flat_map(|index| [format!("{thread} {index}"), "and".into()])
            .collect();
        assert_eq!(texts(&read(store, thread, 0)), expected);
    }
    let shared = read(store, "all", 0);
    assert_eq!(shared.len(), 16 * 40);
    for thread in &threads {
        let prefix = format!("{thread} ");
        let own: Vec<&str> = texts(&shared)
            .into_iter()
            .filter(|text| text.starts_with(&prefix))
            .collect();
        let expected: Vec<String> = (0..40).map(|index| format!("{thread} {index}")).collect();
        assert_eq!(own, expected);
    }
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

fn summary(from: u64) -> Item {
    Item::Summary {
        text: "So far.".into(),
        usage: Usage::default(),
        from,
        hands: BTreeMap::new(),
        last_call: 0,
        forks_left: 64,
        parent: None,
        children: Vec::new(),
    }
}

fn texts(items: &[Item]) -> Vec<&str> {
    items
        .iter()
        .map(|item| match item {
            Item::Prompt { text, .. } => text.as_str(),
            other => panic!("not a prompt: {other:?}"),
        })
        .collect()
}

fn record(mind: &str, origin: Origin) -> MindRecord {
    MindRecord {
        name: name(mind),
        origin,
    }
}

fn read(store: &dyn Store, mind: &str, from: u64) -> Vec<Item> {
    store.read(&name(mind), from, usize::MAX).unwrap()
}

/// Makes a mind with nothing, then appends its first items, as a host does.
fn root(store: &dyn Store, mind: &str, items: &[&str]) {
    store.create(&record(mind, Origin::Root), &[]).unwrap();
    let items: Vec<_> = items.iter().map(|text| prompt(text)).collect();
    store.append(&name(mind), &items).unwrap();
}

fn fork(store: &dyn Store, mind: &str, parent: &str, inherited: u64) {
    let origin = Origin::Fork {
        parent: name(parent),
        inherited,
    };
    store.create(&record(mind, origin), &[]).unwrap();
}

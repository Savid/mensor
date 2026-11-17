//! What every [`Store`] does, as tests any store runs over itself.
//!
//! Each function takes a new, empty store and panics where it differs from
//! what a store must do. [`store_suite!`](crate::store_suite) writes a test
//! for each, so a store's crate runs them all with one line.

use std::collections::BTreeMap;

use mensor_protocol::Name;
use serde_json::json;

use crate::{
    Item, MadeFrom, MindRecord, MindSettings, Origin, Outcome, Stop, Store, StoreError, Usage,
};

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
            a_mind_made_again_with_its_record_is_the_one_made_before,
            minds_are_listed_in_the_order_they_were_made,
            a_store_refuses_what_it_cannot_do,
            an_item_reads_back_as_it_was_stored,
            appends_that_arrive_together_are_each_stored_whole_and_in_order,
            a_mind_made_from_another_shares_its_first_items_and_keeps_its_record,
            a_history_as_it_was_starts_from_its_last_summary_before_then,
            what_a_mind_knows_includes_how_it_began_and_that_it_ended,
            a_lineage_tens_of_generations_deep_reads_whole,
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
    assert_eq!(
        store.last(&name("b7"), "no such kind", u64::MAX).unwrap(),
        None
    );
    assert!(matches!(
        store.last(&name("b7"), "summary", u64::MAX).unwrap(),
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
        left: Some(31),
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

/// A mind made again with the record it was made with is the one made
/// before, with the first items it was made with, and nothing is written.
#[expect(
    clippy::missing_panics_doc,
    reason = "a test: panicking is how it fails"
)]
pub fn a_mind_made_again_with_its_record_is_the_one_made_before(store: &dyn Store) {
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
    assert_eq!(
        store.last(&name("a1"), "outcome", u64::MAX).unwrap(),
        Some(output)
    );
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

/// A mind made from another's first items shares them, as a fork does, and
/// none stored after. It is listed with its origin and its settings, and
/// made again only with the same record.
#[expect(
    clippy::missing_panics_doc,
    reason = "a test: panicking is how it fails"
)]
pub fn a_mind_made_from_another_shares_its_first_items_and_keeps_its_record(store: &dyn Store) {
    root(store, "k", &["0", "1", "2", "3"]);
    let made = MindRecord {
        name: name("r2"),
        origin: Origin::From {
            from: MadeFrom {
                mind: name("k"),
                at: 2,
                hand: name("vm"),
            },
        },
        settings: MindSettings {
            model: Some("claude-haiku-5-5".into()),
            effort: Some("low".into()),
            system: Some("Play on.".into()),
        },
    };
    store.create(&made, &[prompt("r2")]).unwrap();
    store.create(&made, &[prompt("r2")]).unwrap();
    store.append(&name("k"), &[prompt("4")]).unwrap();
    store.append(&name("r2"), &[prompt("r3")]).unwrap();
    assert_eq!(texts(&read(store, "r2", 0)), ["0", "1", "r2", "r3"]);
    assert_eq!(texts(&read(store, "k", 0)), ["0", "1", "2", "3", "4"]);
    assert_eq!(
        store.minds().unwrap(),
        [record("k", Origin::Root), made.clone()]
    );

    let other = MindRecord {
        settings: MindSettings::default(),
        ..made.clone()
    };
    let again = store.create(&other, &[]);
    assert!(matches!(again, Err(StoreError::Exists(_))), "{again:?}");
    let elsewhere = MindRecord {
        origin: Origin::From {
            from: MadeFrom {
                mind: name("k"),
                at: 2,
                hand: name("b2"),
            },
        },
        ..made
    };
    let again = store.create(&elsewhere, &[]);
    assert!(matches!(again, Err(StoreError::Exists(_))), "{again:?}");
    let long = Origin::From {
        from: MadeFrom {
            mind: name("k"),
            at: 6,
            hand: name("vm"),
        },
    };
    let long = store.create(&record("r3", long), &[]);
    assert!(matches!(long, Err(StoreError::Short { .. })), "{long:?}");
}

/// A history as it was when it held some of its items starts from the last
/// summary among them, as a mind made from them is loaded: a summary after
/// them is not one of them.
#[expect(
    clippy::missing_panics_doc,
    reason = "a test: panicking is how it fails"
)]
pub fn a_history_as_it_was_starts_from_its_last_summary_before_then(store: &dyn Store) {
    root(store, "k", &["0", "1"]);
    // 0 and 1, then a summary from 1, 3, a summary from 3, and 5.
    store
        .append(
            &name("k"),
            &[summary(1), prompt("3"), summary(3), prompt("5")],
        )
        .unwrap();
    let tail = |at| {
        let (from, items) = store.tail_at(&name("k"), at).unwrap();
        let said: Vec<String> = items.iter().map(said).map(String::from).collect();
        (from, said)
    };
    assert_eq!(tail(2), (0, owned(&["0", "1"])));
    assert_eq!(tail(4), (1, owned(&["1", "summary", "3"])));
    assert_eq!(tail(6), (3, owned(&["3", "summary", "5"])));
    assert_eq!(tail(u64::MAX), tail(6));
    assert!(matches!(
        store.last(&name("k"), "summary", 4).unwrap(),
        Some(Item::Summary { from: 1, .. })
    ));
    assert_eq!(store.last(&name("k"), "summary", 2).unwrap(), None);

    // A mind made from the first four is loaded from the summary among them.
    let made = Origin::From {
        from: MadeFrom {
            mind: name("k"),
            at: 4,
            hand: name("vm"),
        },
    };
    store.create(&record("r2", made), &[prompt("r4")]).unwrap();
    let (from, items) = store.tail(&name("r2")).unwrap();
    assert_eq!(
        (from, items.iter().map(said).collect::<Vec<_>>()),
        (1, vec!["1", "summary", "3", "r4"])
    );
}

/// What a mind knows of itself holds how it began, when it was made from
/// another's items, and that it ended.
#[expect(
    clippy::missing_panics_doc,
    reason = "a test: panicking is how it fails"
)]
pub fn what_a_mind_knows_includes_how_it_began_and_that_it_ended(store: &dyn Store) {
    root(store, "k", &["0"]);
    let branched = Item::Branched {
        from: name("k"),
        at: 1,
        left: None,
        last_call: 0,
    };
    let made = Origin::From {
        from: MadeFrom {
            mind: name("k"),
            at: 1,
            hand: name("vm"),
        },
    };
    store
        .create(&record("r2", made), &[branched.clone(), prompt("2")])
        .unwrap();
    store.append(&name("r2"), &[Item::Ended]).unwrap();
    assert_eq!(
        store.known(&name("r2")).unwrap(),
        [(1, branched), (3, Item::Ended)]
    );
    assert_eq!(
        store.last(&name("r2"), "ended", u64::MAX).unwrap(),
        Some(Item::Ended)
    );
}

/// A history read through tens of generations, each a fork or a mind made
/// from the one before, reads whole and from anywhere, from its last
/// summary, and finds a key its first mind holds.
#[expect(
    clippy::missing_panics_doc,
    reason = "a test: panicking is how it fails"
)]
pub fn a_lineage_tens_of_generations_deep_reads_whole(store: &dyn Store) {
    let keyed = Item::Prompt {
        key: Some("first".into()),
        text: "g0 0".into(),
        from: None,
    };
    store.create(&record("g0", Origin::Root), &[]).unwrap();
    store.append(&name("g0"), &[keyed, prompt("g0 1")]).unwrap();
    let mut expected: Vec<String> = vec!["g0 0".into(), "g0 1".into()];
    let mut summaries = 0;
    for generation in 1..=40_u64 {
        let parent = format!("g{}", generation - 1);
        // Each shares all of the one before it but its last item, and
        // every seventh none of the last two.
        let shared = expected.len() - 1 - usize::from(generation % 7 == 0);
        expected.truncate(shared);
        let at = u64::try_from(shared).unwrap();
        let origin = if generation % 2 == 0 {
            Origin::Fork {
                parent: name(&parent),
                inherited: at,
            }
        } else {
            Origin::From {
                from: MadeFrom {
                    mind: name(&parent),
                    at,
                    hand: name("vm"),
                },
            }
        };
        let mind = format!("g{generation}");
        store.create(&record(&mind, origin), &[]).unwrap();
        let own = [format!("{mind} 0"), format!("{mind} 1")];
        let items: Vec<Item> = own.iter().map(|text| prompt(text)).collect();
        store.append(&name(&mind), &items).unwrap();
        expected.extend(own);
        // The one before it went on without it.
        store.append(&name(&parent), &[prompt("later")]).unwrap();
        if generation % 10 == 5 {
            // A summary from a third of the way along, and an item after
            // it, which the next generation does not share.
            let from = u64::try_from(expected.len() / 3).unwrap();
            let after = format!("{mind} after");
            store
                .append(&name(&mind), &[summary(from), prompt(&after)])
                .unwrap();
            expected.extend(["summary".into(), after]);
            summaries += 1;
        }
    }
    assert_eq!(summaries, 4);
    let last = name("g40");
    let whole: Vec<String> = read(store, "g40", 0)
        .iter()
        .map(said)
        .map(String::from)
        .collect();
    assert_eq!(whole, expected);
    let middle = expected.len() / 2;
    let from = u64::try_from(middle).unwrap();
    let rest: Vec<String> = read(store, "g40", from)
        .iter()
        .map(said)
        .map(String::from)
        .collect();
    assert_eq!(rest, expected[middle..]);
    let page = store.read(&last, from, 3).unwrap();
    assert_eq!(page.len(), 3);

    // Loaded from its last summary, which a mind made from it shares.
    let at = expected.iter().rposition(|said| said == "summary").unwrap();
    let Some(Item::Summary { from, .. }) = store.last(&last, "summary", u64::MAX).unwrap() else {
        panic!("no summary");
    };
    let (start, tail) = store.tail(&last).unwrap();
    assert_eq!(start, from);
    assert_eq!(tail.len(), expected.len() - usize::try_from(from).unwrap());
    assert!(at >= usize::try_from(from).unwrap());
    assert_eq!(
        store.prompt(&last, "first").unwrap().as_deref(),
        Some("g0 0")
    );
}

fn owned(said: &[&str]) -> Vec<String> {
    said.iter().map(|said| (*said).to_owned()).collect()
}

/// What an item says, as these tests write it: a prompt's text, or a kind.
fn said(item: &Item) -> &str {
    match item {
        Item::Prompt { text, .. } => text,
        other => other.kind(),
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
        forks_left: Some(64),
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
        settings: MindSettings::default(),
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

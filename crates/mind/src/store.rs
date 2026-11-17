use std::collections::BTreeMap;
use std::sync::Mutex;

use mensor_protocol::Name;
use serde::{Deserialize, Serialize};

use crate::Item;

/// Where a mind's history begins.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "origin", rename_all = "snake_case")]
pub enum Origin {
    /// With nothing: a hand said hello to a mind no one had started.
    Root,
    /// With the first `inherited` items of another mind's history, because
    /// that mind forked.
    Fork {
        /// The mind whose history is shared.
        parent: Name,
        /// How many of the parent's items are shared.
        inherited: u64,
    },
}

/// A mind as a store knows it: its name, and where its history begins.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MindRecord {
    /// The mind's name.
    pub name: Name,
    /// Where its history begins.
    #[serde(flatten)]
    pub origin: Origin,
}

/// The kinds of item that say what a mind knows of its hands and its kin.
pub const KNOWING: [&str; 6] = [
    "summary", "attached", "cloned", "released", "forked", "parted",
];

/// What a store says of a mind it is not asked to load, which
/// [`Mind::status`](crate::Mind::status) is told from.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Latest {
    /// The items before `from` that say what the mind knows of its hands
    /// and its kin, each with its index: those a store's
    /// [`known`](Store::known) gives.
    pub known: Vec<(u64, Item)>,
    /// The index the last step had seen, or 0.
    pub from: u64,
    /// The items from there.
    pub items: Vec<Item>,
}

/// Why a store refused.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// A mind of this name already exists, with another origin.
    #[error("a mind named {0} already exists")]
    Exists(Name),
    /// No mind has this name.
    #[error("no mind is named {0}")]
    Missing(Name),
    /// A fork asks for more items than its parent has.
    #[error("{parent} has fewer than the {inherited} items a fork of it would share")]
    Short {
        /// The parent named.
        parent: Name,
        /// The items asked for.
        inherited: u64,
    },
    /// The storage behind the store failed.
    #[error("the store failed: {0}")]
    Backend(String),
}

/// Where histories are kept.
///
/// A history only grows, and a fork shares its parent's items without copying
/// them, so a store never changes or removes an item another mind may read.
/// An append is durable when it returns: a mind's host relies on that before
/// it sends anything the appended items describe.
///
/// A store holds every item, and a mind only the end of its history, so a
/// store also answers what a mind no longer can.
pub trait Store: Send + Sync {
    /// Starts a mind's history with `items`, in one write. A mind that
    /// already exists with the same origin was made by an earlier call that
    /// was not seen to finish: it counts as made, and nothing is written.
    fn create(&self, mind: &MindRecord, items: &[Item]) -> Result<(), StoreError>;

    /// Adds items to the end of a mind's history.
    fn append(&self, name: &Name, items: &[Item]) -> Result<(), StoreError>;

    /// A mind's history from index `from`, shared items included, `limit`
    /// items of it at the most.
    fn read(&self, name: &Name, from: u64, limit: usize) -> Result<Vec<Item>, StoreError>;

    /// The last item of one [kind](Item::kind) in a mind's history.
    fn last(&self, name: &Name, kind: &str) -> Result<Option<Item>, StoreError>;

    /// The end of a mind's history, which is what its mind is loaded with:
    /// the index its last summary names and the items from there, or all
    /// of it from 0 when it holds no summary.
    fn tail(&self, name: &Name) -> Result<(u64, Vec<Item>), StoreError> {
        let from = match self.last(name, "summary")? {
            Some(Item::Summary { from, .. }) => from,
            _ => 0,
        };
        Ok((from, self.read(name, from, usize::MAX)?))
    }

    /// The latest of a mind's history, which is what [`Mind::status`] is
    /// told from: the items from the index its last step had seen, or all
    /// of it from 0 when it holds no step, and what is [`known`](Store::known)
    /// from before them. That is the round in progress, the prompts no step
    /// has answered, and the mind's hands and kin, however much the mind
    /// would hold.
    ///
    /// [`Mind::status`]: crate::Mind::status
    fn recent(&self, name: &Name) -> Result<Latest, StoreError> {
        let from = match self.last(name, "step")? {
            Some(Item::Step { seen, .. }) => seen,
            _ => 0,
        };
        let known = self.known(name)?;
        let known = known.into_iter().filter(|(at, _)| *at < from).collect();
        Ok(Latest {
            known,
            from,
            items: self.read(name, from, usize::MAX)?,
        })
    }

    /// The items that say what a mind knows of its hands and its kin, each
    /// with its index: its last summary and every item of the
    /// [kinds](KNOWING) from the index that summary names, or from 0.
    fn known(&self, name: &Name) -> Result<Vec<(u64, Item)>, StoreError> {
        let (from, items) = self.tail(name)?;
        let known = (from..).zip(items);
        Ok(known
            .filter(|(_, item)| KNOWING.contains(&item.kind()))
            .collect())
    }

    /// The text of the prompt a mind's history holds under `key`.
    fn prompt(&self, name: &Name, key: &str) -> Result<Option<String>, StoreError>;

    /// Every mind, parents before their forks.
    fn minds(&self) -> Result<Vec<MindRecord>, StoreError>;
}

/// The index a mind starts from in the history `items`: the one its last
/// summary names, or 0.
pub fn tail_from(items: &[Item]) -> u64 {
    let from = items.iter().rev().find_map(|item| match item {
        Item::Summary { from, .. } => Some(*from),
        _ => None,
    });
    from.unwrap_or(0)
}

/// The text of the prompt `items` hold under `key`.
pub fn prompt_under<'a>(items: &'a [Item], key: &str) -> Option<&'a str> {
    items.iter().find_map(|item| match item {
        Item::Prompt {
            key: Some(held),
            text,
            ..
        } if held == key => Some(text.as_str()),
        _ => None,
    })
}

/// A store that keeps everything in memory, for tests and for hosts that
/// need nothing to survive them.
#[derive(Debug, Default)]
pub struct MemoryStore {
    minds: Mutex<Minds>,
}

#[derive(Debug, Default)]
struct Minds {
    by_name: BTreeMap<Name, Entry>,
    order: Vec<Name>,
}

#[derive(Debug)]
struct Entry {
    origin: Origin,
    own: Vec<Item>,
}

impl MemoryStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Minds> {
        self.minds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Minds {
    /// The first `limit` items of a mind's history.
    fn history(&self, name: &Name, limit: u64) -> Result<Vec<Item>, StoreError> {
        let entry = self
            .by_name
            .get(name)
            .ok_or_else(|| StoreError::Missing(name.clone()))?;
        let (mut items, base) = match &entry.origin {
            Origin::Root => (Vec::new(), 0),
            Origin::Fork { parent, inherited } => {
                (self.history(parent, limit.min(*inherited))?, *inherited)
            }
        };
        let own = usize::try_from(limit.saturating_sub(base)).unwrap_or(usize::MAX);
        items.extend(entry.own.iter().take(own).cloned());
        Ok(items)
    }
}

impl Store for MemoryStore {
    fn create(&self, mind: &MindRecord, items: &[Item]) -> Result<(), StoreError> {
        let mut minds = self.lock();
        if let Some(entry) = minds.by_name.get(&mind.name) {
            return if entry.origin == mind.origin {
                Ok(())
            } else {
                Err(StoreError::Exists(mind.name.clone()))
            };
        }
        if let Origin::Fork { parent, inherited } = &mind.origin {
            let have = minds.history(parent, u64::MAX)?.len();
            if u64::try_from(have).unwrap_or(u64::MAX) < *inherited {
                return Err(StoreError::Short {
                    parent: parent.clone(),
                    inherited: *inherited,
                });
            }
        }
        minds.by_name.insert(
            mind.name.clone(),
            Entry {
                origin: mind.origin.clone(),
                own: items.to_vec(),
            },
        );
        minds.order.push(mind.name.clone());
        Ok(())
    }

    fn append(&self, name: &Name, items: &[Item]) -> Result<(), StoreError> {
        let mut minds = self.lock();
        let entry = minds
            .by_name
            .get_mut(name)
            .ok_or_else(|| StoreError::Missing(name.clone()))?;
        entry.own.extend_from_slice(items);
        Ok(())
    }

    fn read(&self, name: &Name, from: u64, limit: usize) -> Result<Vec<Item>, StoreError> {
        let mut items = self.lock().history(name, u64::MAX)?;
        let from = usize::try_from(from).unwrap_or(usize::MAX).min(items.len());
        let mut items = items.split_off(from);
        items.truncate(limit);
        Ok(items)
    }

    fn last(&self, name: &Name, kind: &str) -> Result<Option<Item>, StoreError> {
        let items = self.lock().history(name, u64::MAX)?;
        Ok(items.into_iter().rev().find(|item| item.kind() == kind))
    }

    fn prompt(&self, name: &Name, key: &str) -> Result<Option<String>, StoreError> {
        let items = self.lock().history(name, u64::MAX)?;
        Ok(prompt_under(&items, key).map(str::to_owned))
    }

    fn minds(&self) -> Result<Vec<MindRecord>, StoreError> {
        let minds = self.lock();
        Ok(minds
            .order
            .iter()
            .map(|name| MindRecord {
                name: name.clone(),
                origin: minds.by_name[name].origin.clone(),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::store_suite!(MemoryStore::new());

    #[test]
    fn a_mind_is_written_flat() {
        let mind = MindRecord {
            name: Name::new("b7").unwrap(),
            origin: Origin::Fork {
                parent: Name::new("a1").unwrap(),
                inherited: 4,
            },
        };
        let written = serde_json::to_value(&mind).unwrap();
        assert_eq!(
            written,
            serde_json::json!({"name": "b7", "origin": "fork", "parent": "a1", "inherited": 4})
        );
        assert_eq!(serde_json::from_value::<MindRecord>(written).unwrap(), mind);
    }
}

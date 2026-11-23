use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use mensor_protocol::Name;
use serde::{Deserialize, Serialize};

use crate::Item;

/// Where a mind's history begins.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "origin", rename_all = "snake_case")]
pub enum Origin {
    /// With nothing: it was made so, or a hand said hello to a mind no one
    /// had made.
    Root,
    /// With the first `inherited` items of another mind's history, because
    /// that mind forked.
    Fork {
        /// The mind whose history is shared.
        parent: Name,
        /// How many of the parent's items are shared.
        inherited: u64,
    },
    /// With the first items of another mind's history, because it was made
    /// from them, keeping one of that mind's hands. It is no kin of it.
    From {
        /// Which items, and which hand.
        from: MadeFrom,
    },
}

/// The history a mind was made from: the first `at` items of `mind`'s,
/// keeping its hand `hand`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MadeFrom {
    /// The mind whose history is shared.
    pub mind: Name,
    /// How many of its items are shared: to start just after an item, its
    /// index plus one.
    pub at: u64,
    /// The hand of that mind's it keeps.
    pub hand: Name,
}

impl Origin {
    /// The mind whose items this history starts with, and how many of
    /// them: none for a root.
    pub fn shares(&self) -> Option<(&Name, u64)> {
        match self {
            Self::Root => None,
            Self::Fork { parent, inherited } => Some((parent, *inherited)),
            Self::From { from } => Some((&from.mind, from.at)),
        }
    }
}

/// What a mind is made with, beyond what its host gives every mind. A
/// setting left out is the host's.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MindSettings {
    /// The model it asks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// How hard that model works, in its API's words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// The system prompt it gives its model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
}

impl MindSettings {
    /// These, with each that is left out taken from `under`.
    #[must_use]
    pub fn or(self, under: &Self) -> Self {
        Self {
            model: self.model.or_else(|| under.model.clone()),
            effort: self.effort.or_else(|| under.effort.clone()),
            system: self.system.or_else(|| under.system.clone()),
        }
    }

    /// What a store keeps these under, once however many minds share
    /// them: a SHA-256 of them, in hex.
    pub fn digest(&self) -> String {
        use sha2::Digest;
        use std::fmt::Write;
        let mut hash = sha2::Sha256::new();
        // Whether each is set, and its length, so that no two settings
        // are hashed alike.
        for part in [&self.model, &self.effort, &self.system] {
            hash.update([u8::from(part.is_some())]);
            let part = part.as_deref().unwrap_or_default().as_bytes();
            hash.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_le_bytes());
            hash.update(part);
        }
        let mut digest = String::with_capacity(64);
        for byte in hash.finalize() {
            let _ = write!(digest, "{byte:02x}");
        }
        digest
    }
}

/// A mind as a store knows it: its name, where its history begins, and what
/// it was made with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MindRecord {
    /// The mind's name.
    pub name: Name,
    /// Where its history begins.
    #[serde(flatten)]
    pub origin: Origin,
    /// What it was made with, shared with every mind made the same way.
    #[serde(flatten)]
    pub settings: Arc<MindSettings>,
}

impl MindRecord {
    /// A root named `name`, made with nothing of its own.
    pub fn root(name: Name) -> Self {
        Self {
            name,
            origin: Origin::Root,
            settings: Arc::default(),
        }
    }
}

/// The kinds of item that say what a mind knows of its hands, its kin and
/// itself.
pub const KNOWING: [&str; 8] = [
    "summary", "attached", "cloned", "released", "forked", "parted", "branched", "ended",
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
    /// A mind of this name already exists, with another origin or other
    /// settings.
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
    /// already exists with the same record was made by an earlier call that
    /// was not seen to finish: it counts as made, and nothing is written.
    fn create(&self, mind: &MindRecord, items: &[Item]) -> Result<(), StoreError>;

    /// Adds items to the end of a mind's history.
    fn append(&self, name: &Name, items: &[Item]) -> Result<(), StoreError>;

    /// A mind's history from index `from`, shared items included, `limit`
    /// items of it at the most.
    fn read(&self, name: &Name, from: u64, limit: usize) -> Result<Vec<Item>, StoreError>;

    /// The last item of one [kind](Item::kind) among the first `before`
    /// items of a mind's history: `u64::MAX` for all of it.
    fn last(&self, name: &Name, kind: &str, before: u64) -> Result<Option<Item>, StoreError>;

    /// The end of a mind's history, which is what its mind is loaded with:
    /// the index its last summary names and the items from there, or all
    /// of it from 0 when it holds no summary.
    fn tail(&self, name: &Name) -> Result<(u64, Vec<Item>), StoreError> {
        self.tail_at(name, u64::MAX)
    }

    /// The end of a mind's history as it was when it held `at` items: the
    /// index its last summary before then names, and the items from there
    /// up to `at`. A mind made from those `at` items is loaded with this,
    /// and its own after it.
    fn tail_at(&self, name: &Name, at: u64) -> Result<(u64, Vec<Item>), StoreError> {
        let from = match self.last(name, "summary", at)? {
            Some(Item::Summary { from, .. }) => from,
            _ => 0,
        };
        let limit = usize::try_from(at.saturating_sub(from)).unwrap_or(usize::MAX);
        Ok((from, self.read(name, from, limit)?))
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
        let from = match self.last(name, "step", u64::MAX)? {
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

    /// Every mind that has not ended, in the order they were made: those
    /// whose own items hold no [`Item::Ended`].
    fn live(&self) -> Result<Vec<Name>, StoreError>;

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
    record: MindRecord,
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
        let (mut items, base) = match entry.record.origin.shares() {
            None => (Vec::new(), 0),
            Some((parent, inherited)) => (self.history(parent, limit.min(inherited))?, inherited),
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
            return if entry.record == *mind {
                Ok(())
            } else {
                Err(StoreError::Exists(mind.name.clone()))
            };
        }
        if let Some((parent, inherited)) = mind.origin.shares() {
            let have = minds.history(parent, u64::MAX)?.len();
            if u64::try_from(have).unwrap_or(u64::MAX) < inherited {
                return Err(StoreError::Short {
                    parent: parent.clone(),
                    inherited,
                });
            }
        }
        minds.by_name.insert(
            mind.name.clone(),
            Entry {
                record: mind.clone(),
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

    fn last(&self, name: &Name, kind: &str, before: u64) -> Result<Option<Item>, StoreError> {
        let items = self.lock().history(name, before)?;
        Ok(items.into_iter().rev().find(|item| item.kind() == kind))
    }

    fn prompt(&self, name: &Name, key: &str) -> Result<Option<String>, StoreError> {
        let items = self.lock().history(name, u64::MAX)?;
        Ok(prompt_under(&items, key).map(str::to_owned))
    }

    fn live(&self) -> Result<Vec<Name>, StoreError> {
        let minds = self.lock();
        let ended = |name: &Name| minds.by_name[name].own.contains(&Item::Ended);
        Ok(minds
            .order
            .iter()
            .filter(|name| !ended(name))
            .cloned()
            .collect())
    }

    fn minds(&self) -> Result<Vec<MindRecord>, StoreError> {
        let minds = self.lock();
        Ok(minds
            .order
            .iter()
            .map(|name| minds.by_name[name].record.clone())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::store_suite!(MemoryStore::new());

    #[test]
    fn a_mind_is_written_flat() {
        let name = |name: &str| Name::new(name).unwrap();
        let fork = MindRecord {
            name: name("b7"),
            origin: Origin::Fork {
                parent: name("a1"),
                inherited: 4,
            },
            settings: Arc::default(),
        };
        let made = MindRecord {
            name: name("r2"),
            origin: Origin::From {
                from: MadeFrom {
                    mind: name("k"),
                    at: 40,
                    hand: name("vm"),
                },
            },
            settings: Arc::new(MindSettings {
                model: Some("claude-haiku-5-5".into()),
                effort: None,
                system: Some("Play.".into()),
            }),
        };
        let cases = [
            (
                fork,
                serde_json::json!({"name": "b7", "origin": "fork", "parent": "a1", "inherited": 4}),
            ),
            (
                made,
                serde_json::json!({"name": "r2", "origin": "from",
                                   "from": {"mind": "k", "at": 40, "hand": "vm"},
                                   "model": "claude-haiku-5-5", "system": "Play."}),
            ),
        ];
        for (mind, expected) in cases {
            let written = serde_json::to_value(&mind).unwrap();
            assert_eq!(written, expected);
            assert_eq!(serde_json::from_value::<MindRecord>(written).unwrap(), mind);
        }
    }

    #[test]
    fn a_setting_left_out_is_taken_from_under_it() {
        let own = MindSettings {
            model: Some("a".into()),
            ..MindSettings::default()
        };
        let under = MindSettings {
            model: Some("b".into()),
            effort: Some("high".into()),
            system: None,
        };
        assert_eq!(
            own.or(&under),
            MindSettings {
                model: Some("a".into()),
                effort: Some("high".into()),
                system: None,
            }
        );
    }
}

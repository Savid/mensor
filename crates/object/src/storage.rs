use std::cell::RefCell;
use std::rc::Rc;

use mensor_hand::Core;
use mensor_mind::{Item, prompt_under, tail_from};

/// Why an object's storage could not do something.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct StorageError(pub String);

/// What an object is opened with.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stored {
    /// The index of the first of `items` in the history.
    pub from: u64,
    /// The end of the history, oldest first: the items from the index its
    /// last summary names, or all of it when it holds no summary.
    pub items: Vec<Item>,
    /// What the hand remembered, if a hand was ever kept.
    pub hand: Option<Core>,
}

/// Where one object keeps its history and what its hand remembers.
///
/// An object writes in a fixed order: an item is appended before anything
/// it describes is done. A storage that commits several writes together
/// only makes that stronger: a call made between two commits is then stored
/// with its outcome or not at all.
///
/// A storage holds every item, and an object only the end of its history,
/// so a storage also answers what the object no longer can.
pub trait Storage {
    /// The end of the history, and what the hand remembers.
    fn load(&mut self) -> Result<Stored, StorageError>;

    /// Adds these items to the end of the history.
    fn append(&mut self, items: &[Item]) -> Result<(), StorageError>;

    /// Replaces what the hand remembers.
    fn keep(&mut self, hand: &Core) -> Result<(), StorageError>;

    /// The history from index `from`, `limit` items of it at the most.
    fn read(&self, from: u64, limit: usize) -> Result<Vec<Item>, StorageError>;

    /// The text of the prompt the history holds under `key`.
    fn prompt(&self, key: &str) -> Result<Option<String>, StorageError>;
}

/// A storage in memory, for tests and for trying an object without a
/// database. Its clones share what is stored, so an object opened over a
/// clone carries on where the last one stopped.
#[derive(Clone, Debug, Default)]
pub struct MemoryStorage {
    stored: Rc<RefCell<Stored>>,
}

impl MemoryStorage {
    /// A storage that holds nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything stored so far: the whole history, from its first item.
    pub fn stored(&self) -> Stored {
        self.stored.borrow().clone()
    }
}

impl Storage for MemoryStorage {
    fn load(&mut self) -> Result<Stored, StorageError> {
        let mut stored = self.stored();
        stored.from = tail_from(&stored.items);
        stored.items = self.read(stored.from, usize::MAX)?;
        Ok(stored)
    }

    fn append(&mut self, items: &[Item]) -> Result<(), StorageError> {
        self.stored.borrow_mut().items.extend_from_slice(items);
        Ok(())
    }

    fn keep(&mut self, hand: &Core) -> Result<(), StorageError> {
        self.stored.borrow_mut().hand = Some(hand.clone());
        Ok(())
    }

    fn read(&self, from: u64, limit: usize) -> Result<Vec<Item>, StorageError> {
        let stored = self.stored.borrow();
        let from = usize::try_from(from).unwrap_or(usize::MAX);
        let items = stored.items.iter().skip(from).take(limit);
        Ok(items.cloned().collect())
    }

    fn prompt(&self, key: &str) -> Result<Option<String>, StorageError> {
        let stored = self.stored.borrow();
        Ok(prompt_under(&stored.items, key).map(str::to_owned))
    }
}

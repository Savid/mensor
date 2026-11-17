//! What a fork makes: its children's names, their shares of the fork
//! budget, and the first items of each.

use mensor_protocol::{CallId, Name};

use crate::Item;

/// A child mind for the host to make, before it performs anything after.
///
/// The child shares the first `inherited` items of its parent's history,
/// which end with the call that cloned its hand, and starts with `items` of
/// its own. A host that makes a child it already made, because it stopped
/// before the parent stored the fork, finds it made.
#[derive(Clone, Debug, PartialEq)]
pub struct Fork {
    /// The child's name.
    pub name: Name,
    /// Its parent.
    pub parent: Name,
    /// How many of the parent's items it shares.
    pub inherited: u64,
    /// Its first items of its own.
    pub items: Vec<Item>,
}

/// The name of child `index` of a fork by the call `call` of the mind
/// `parent`: `<root>.<code>`, where `<root>` is `parent` up to its first `.`
/// and `<code>` eight base-32 characters of a hash of all three. The same
/// fork names its children the same way every time, and a name stays the
/// same length at any depth.
pub(crate) fn child_name(parent: &Name, call: CallId, index: usize) -> Option<Name> {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    // FNV-1a, 64 bits: fixed, so a name never depends on a build.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let index = u64::try_from(index).unwrap_or(u64::MAX);
    let bytes = parent.as_str().bytes().chain([0]);
    for byte in bytes.chain(call.to_le_bytes()).chain(index.to_le_bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let code: String = (0..8)
        .map(|at| char::from(ALPHABET[usize::try_from((hash >> (at * 5)) & 31).unwrap_or(0)]))
        .collect();
    Name::new(format!("{}.{code}", parent.root())).ok()
}

/// How a mind with `left` forks shares what remains after it forks
/// `children`: each child's share, and its own. What remains is split
/// evenly between the mind and its children, the mind keeping what does not
/// divide. `children` is at most `left`. A mind with no limit gives its
/// children none either: nothing is split.
pub(crate) fn shares(left: Option<u64>, children: u64) -> (Option<u64>, Option<u64>) {
    let Some(left) = left else {
        return (None, None);
    };
    let remains = left.saturating_sub(children);
    let share = remains / (children + 1);
    (Some(share), Some(remains - share * children))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(name: &str) -> Name {
        Name::new(name).unwrap()
    }

    #[test]
    fn a_child_is_named_from_its_root_the_same_way_every_time() {
        let first = child_name(&name("m"), 7, 0).unwrap();
        assert_eq!(first, child_name(&name("m"), 7, 0).unwrap());
        assert_ne!(first, child_name(&name("m"), 7, 1).unwrap());
        assert_ne!(first, child_name(&name("m"), 8, 0).unwrap());
        assert!(first.as_str().starts_with("m.") && first.as_str().len() == 10);

        // A grandchild keeps the root and the length.
        let grandchild = child_name(&first, 3, 0).unwrap();
        assert!(grandchild.as_str().starts_with("m.") && grandchild.as_str().len() == 10);
        assert_ne!(grandchild, first);

        let longest = name(&"x".repeat(Name::ROOT_MAX_LEN));
        assert_eq!(
            child_name(&longest, 1, 0).unwrap().as_str().len(),
            Name::MAX_LEN
        );
    }

    #[test]
    fn what_remains_of_a_budget_is_split_evenly() {
        assert_eq!(shares(Some(64), 2), (Some(20), Some(22)));
        assert_eq!(shares(Some(64), 1), (Some(31), Some(32)));
        assert_eq!(shares(Some(3), 3), (Some(0), Some(0)));
        assert_eq!(shares(Some(10), 8), (Some(0), Some(2)));
    }

    #[test]
    fn no_limit_is_never_split() {
        assert_eq!(shares(None, 8), (None, None));
        // However many generations take over from the one before.
        let mut left = None;
        for _ in 0..100 {
            left = shares(left, 8).0;
        }
        assert_eq!(left, None);
    }
}

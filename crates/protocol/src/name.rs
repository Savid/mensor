use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// The name of a mind, or of a hand within its mind.
///
/// A mind that starts from nothing has a root name: no `.`, and at most
/// [`Name::ROOT_MAX_LEN`] bytes. A mind made by a fork is named
/// `<root>.<code>`, so a `.` marks a fork child, and its name fits in
/// [`Name::MAX_LEN`] however deep the fork.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Name(String);

/// Why a string is not a [`Name`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InvalidName {
    /// The string is empty.
    #[error("a name cannot be empty")]
    Empty,
    /// The string is longer than [`Name::MAX_LEN`] bytes.
    #[error("a name cannot be longer than {} bytes", Name::MAX_LEN)]
    TooLong,
    /// The string holds a character a name cannot.
    #[error(
        "a name holds only ASCII letters, digits, '-', '_' and '.', and starts with a letter or digit"
    )]
    Character,
}

impl Name {
    /// The longest a name can be, in bytes.
    pub const MAX_LEN: usize = 64;

    /// The longest a root name can be, in bytes: room for a `.` and the
    /// eight characters of a fork child's code.
    pub const ROOT_MAX_LEN: usize = 55;

    /// Checks `name` and wraps it.
    ///
    /// A name is 1 to [`Name::MAX_LEN`] ASCII letters, digits, `-`, `_` or
    /// `.`, starting with a letter or digit, so it is safe in a URL path.
    pub fn new(name: impl Into<String>) -> Result<Self, InvalidName> {
        let name = name.into();
        let Some(first) = name.bytes().next() else {
            return Err(InvalidName::Empty);
        };
        if name.len() > Self::MAX_LEN {
            return Err(InvalidName::TooLong);
        }
        let allowed = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.');
        if !first.is_ascii_alphanumeric() || !name.bytes().all(allowed) {
            return Err(InvalidName::Character);
        }
        Ok(Self(name))
    }

    /// The name as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this can name a mind that starts from nothing: it has no `.`
    /// and is at most [`Name::ROOT_MAX_LEN`] bytes.
    pub fn is_root(&self) -> bool {
        !self.0.contains('.') && self.0.len() <= Self::ROOT_MAX_LEN
    }

    /// The root this name descends from: all of it up to its first `.`.
    pub fn root(&self) -> &str {
        self.0.split('.').next().unwrap_or(&self.0)
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for Name {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl FromStr for Name {
    type Err = InvalidName;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl TryFrom<String> for Name {
    type Error = InvalidName;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<Name> for String {
    fn from(name: Name) -> Self {
        name.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_url_safe_names() {
        for ok in [
            "a1",
            "b7",
            "build-box",
            "A.b_c-9",
            &"x".repeat(Name::MAX_LEN),
        ] {
            assert_eq!(Name::new(ok).unwrap().as_str(), ok);
        }
    }

    #[test]
    fn refuses_everything_else() {
        assert_eq!(Name::new(""), Err(InvalidName::Empty));
        assert_eq!(
            Name::new("x".repeat(Name::MAX_LEN + 1)),
            Err(InvalidName::TooLong)
        );
        for bad in [".", "..", "-a", "a/b", "a b", "ä", "a\n"] {
            assert_eq!(Name::new(bad), Err(InvalidName::Character), "{bad:?}");
        }
    }

    #[test]
    fn a_root_has_no_dot_and_room_for_a_child_code() {
        assert!(Name::new("m").unwrap().is_root());
        assert!(Name::new("x".repeat(Name::ROOT_MAX_LEN)).unwrap().is_root());
        assert!(
            !Name::new("x".repeat(Name::ROOT_MAX_LEN + 1))
                .unwrap()
                .is_root()
        );
        let child = Name::new("m.d3342iuj").unwrap();
        assert!(!child.is_root());
        assert_eq!(child.root(), "m");
    }

    #[test]
    fn checks_a_name_as_it_is_decoded() {
        assert_eq!(
            serde_json::from_str::<Name>("\"a1\"").unwrap().as_str(),
            "a1"
        );
        assert!(serde_json::from_str::<Name>("\"a/b\"").is_err());
        assert_eq!(
            serde_json::to_string(&Name::new("a1").unwrap()).unwrap(),
            "\"a1\""
        );
    }
}

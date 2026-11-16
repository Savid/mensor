use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A hand's name.
///
/// A name is opaque to the mind, which neither issues nor checks one: it only
/// asks whether it has seen the name before. Whoever starts or clones a hand
/// feeds it its name. Because a mind and its hand are always a pair, the name
/// also identifies the pair.
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

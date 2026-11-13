/// Why a mind's host ends a connection.
///
/// Each refusal is a WebSocket close code in the range kept for applications,
/// so a body can tell a refusal that will never change from one worth another
/// try.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The body sent something the protocol does not define.
    Protocol,
    /// The body speaks another version of the protocol.
    Version,
    /// The lineage already has as many clones as it may.
    Limit,
    /// The body says it was cloned from a body the host does not know.
    UnknownParent,
    /// A newer connection under the same name took this one's place.
    Replaced,
    /// The body is not a clone of the body it names: it has read a call that
    /// body's mind never made.
    Stranger,
}

impl Refusal {
    /// Every refusal.
    pub const ALL: [Self; 6] = [
        Self::Protocol,
        Self::Version,
        Self::Limit,
        Self::UnknownParent,
        Self::Replaced,
        Self::Stranger,
    ];

    /// The WebSocket close code.
    pub const fn code(self) -> u16 {
        match self {
            Self::Protocol => 4000,
            Self::Version => 4001,
            Self::Limit => 4002,
            Self::UnknownParent => 4003,
            Self::Replaced => 4004,
            Self::Stranger => 4005,
        }
    }

    /// The close reason, for a log.
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Protocol => "not a message of the protocol",
            Self::Version => "another version of the protocol",
            Self::Limit => "the lineage has all the clones it may",
            Self::UnknownParent => "cloned from a body this host does not know",
            Self::Replaced => "a newer connection has this name",
            Self::Stranger => "not a clone of the body it names",
        }
    }

    /// The refusal a close code names.
    pub fn from_code(code: u16) -> Option<Self> {
        Self::ALL.into_iter().find(|refusal| refusal.code() == code)
    }

    /// Whether dialling again could end differently. The body a clone was
    /// made from may yet connect; the others stay as they are until someone
    /// changes the body or the host.
    pub const fn is_final(self) -> bool {
        !matches!(self, Self::UnknownParent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_distinct_and_in_the_application_range() {
        for refusal in Refusal::ALL {
            assert!((4000..5000).contains(&refusal.code()));
            assert_eq!(Refusal::from_code(refusal.code()), Some(refusal));
        }
        assert_eq!(Refusal::from_code(1000), None);
    }

    #[test]
    fn only_an_unknown_parent_is_worth_another_dial() {
        for refusal in Refusal::ALL {
            assert_eq!(refusal.is_final(), refusal != Refusal::UnknownParent);
        }
    }
}

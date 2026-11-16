/// Why a mind's host ends a hand's connection.
///
/// Each refusal is a WebSocket close code in the range kept for applications,
/// so a hand can tell a refusal that will never change from one worth another
/// try.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The hand sent something the protocol does not define.
    Protocol,
    /// The mind has all the hands it may.
    Limit,
    /// The hand names a fork child that is not made yet.
    UnknownMind,
    /// A newer connection for the same mind and hand took this one's place.
    Replaced,
    /// The mind released this hand.
    Released,
    /// The hand names a mind that cannot be started: a new mind's name has
    /// no `.` and is at most [`Name::ROOT_MAX_LEN`](crate::Name::ROOT_MAX_LEN)
    /// bytes.
    Name,
}

impl Refusal {
    /// Every refusal.
    pub const ALL: [Self; 6] = [
        Self::Protocol,
        Self::Limit,
        Self::UnknownMind,
        Self::Replaced,
        Self::Released,
        Self::Name,
    ];

    /// The WebSocket close code.
    pub const fn code(self) -> u16 {
        match self {
            Self::Protocol => 4000,
            Self::Limit => 4001,
            Self::UnknownMind => 4002,
            Self::Replaced => 4003,
            Self::Released => 4004,
            Self::Name => 4005,
        }
    }

    /// The close reason, for a log.
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Protocol => "not a message of the protocol",
            Self::Limit => "the mind has all the hands it may",
            Self::UnknownMind => "no mind of that name yet",
            Self::Replaced => "a newer connection has this mind and hand",
            Self::Released => "the mind released this hand",
            Self::Name => "a new mind's name has no '.' and is at most 55 bytes",
        }
    }

    /// The refusal a close code names.
    pub fn from_code(code: u16) -> Option<Self> {
        Self::ALL.into_iter().find(|refusal| refusal.code() == code)
    }

    /// Whether dialling again could end differently. A fork child is made
    /// once its parent stores the fork; the others stay as they are until
    /// someone changes the hand or the host.
    pub const fn is_final(self) -> bool {
        !matches!(self, Self::UnknownMind)
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
    }

    #[test]
    fn only_a_mind_not_made_yet_is_worth_another_try() {
        let retried: Vec<_> = Refusal::ALL
            .into_iter()
            .filter(|refusal| !refusal.is_final())
            .collect();
        assert_eq!(retried, [Refusal::UnknownMind]);
    }
}

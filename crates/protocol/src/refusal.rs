/// Why a mind's host ends a hand's connection.
///
/// Each refusal is a WebSocket close code in the range kept for applications,
/// so a hand can tell a refusal that will never change from one worth another
/// try.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The hand sent something the protocol does not define.
    Protocol,
    /// The hand names a fork child that is not made yet.
    UnknownMind,
    /// A newer connection for the same mind and hand took this one's place.
    Replaced,
    /// The mind released this hand, or has ended and takes none.
    Released,
    /// The hand names a mind that cannot be started: a new mind's name has
    /// no `.` and is at most [`Name::ROOT_MAX_LEN`](crate::Name::ROOT_MAX_LEN)
    /// bytes.
    Name,
    /// The hand's token is for another mind: a token made for one mind
    /// reaches only that mind and its children.
    Forbidden,
}

impl Refusal {
    /// Every refusal.
    pub const ALL: [Self; 6] = [
        Self::Protocol,
        Self::UnknownMind,
        Self::Replaced,
        Self::Released,
        Self::Name,
        Self::Forbidden,
    ];

    /// The WebSocket close code. 4001 is not used.
    pub const fn code(self) -> u16 {
        match self {
            Self::Protocol => 4000,
            Self::UnknownMind => 4002,
            Self::Replaced => 4003,
            Self::Released => 4004,
            Self::Name => 4005,
            Self::Forbidden => 4006,
        }
    }

    /// The close reason, for a log.
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Protocol => "not a message of the protocol",
            Self::UnknownMind => "no mind of that name yet",
            Self::Replaced => "a newer connection has this mind and hand",
            Self::Released => "the mind released this hand, or has ended",
            Self::Name => "a new mind's name has no '.' and is at most 55 bytes",
            Self::Forbidden => "this hand's token does not reach that mind",
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
    fn refusals_close_with_the_codes_the_protocol_names_and_only_4002_is_worth_another_try() {
        let codes: Vec<(u16, bool)> = Refusal::ALL
            .into_iter()
            .map(|refusal| {
                assert_eq!(Refusal::from_code(refusal.code()), Some(refusal));
                (refusal.code(), refusal.is_final())
            })
            .collect();
        assert_eq!(
            codes,
            [
                (4000, true),
                (4002, false),
                (4003, true),
                (4004, true),
                (4005, true),
                (4006, true)
            ]
        );
    }
}

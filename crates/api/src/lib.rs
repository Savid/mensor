//! The HTTP API's shapes, which every host of minds serves alike.
//!
//! `mensor serve` and the Cloudflare worker answer the same requests under
//! `/v1/minds` with the same JSON, so a client reads either the same way.
//! These are the bodies of those requests and answers, and the check of a
//! bearer token. How a request is routed and how an answer is sent stay
//! with each host.
//!
//! This crate is pure: no I/O, no clock and no randomness.

use mensor_mind::{Item, MindRecord, State};
use mensor_protocol::Name;
use serde::{Deserialize, Serialize};

/// A refusal, as RFC 9457 problem details.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Problem {
    /// The HTTP status the refusal is sent with.
    pub status: u16,
    /// What was wrong, for a person.
    pub detail: String,
}

impl Problem {
    /// The content type a problem is sent as.
    pub const CONTENT_TYPE: &'static str = "application/problem+json";

    /// A refusal with this status.
    pub fn new(status: u16, detail: impl Into<String>) -> Self {
        Self {
            status,
            detail: detail.into(),
        }
    }
}

/// The body of `POST /v1/minds/{name}/prompt`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct PromptBody {
    /// The caller's key. A prompt under a key the history already holds is
    /// that prompt, not a new one.
    #[serde(default)]
    pub key: Option<String>,
    /// What to say to the mind.
    pub text: String,
}

/// What became of a prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Prompted {
    /// It is stored, and the mind will answer it.
    Accepted,
    /// The history already held it under its key.
    Existing,
}

impl Prompted {
    /// The HTTP status the answer is sent with.
    pub const fn status(self) -> u16 {
        match self {
            Self::Accepted => 202,
            Self::Existing => 200,
        }
    }
}

/// The answer to a prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct PromptReply {
    /// What became of the prompt.
    pub status: Prompted,
}

/// A mind and what it is doing: the answer to `GET /v1/minds/{name}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MindStatus {
    /// The mind and where its history begins.
    #[serde(flatten)]
    pub mind: MindRecord,
    /// What it is doing.
    pub state: State,
    /// Its hands that are connected.
    pub hands: Vec<Name>,
    /// The hands its turn waits for, because they are not connected.
    pub waiting: Vec<Name>,
    /// The children a `wait` of its turn waits for.
    pub watching: Vec<Name>,
    /// How many items its history holds.
    pub length: u64,
}

/// The body of `POST /v1/minds/{name}/release`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ReleaseBody {
    /// The hand to let go.
    pub hand: Name,
}

/// The most items one answer to `GET /v1/minds/{name}/history` holds. A
/// reader carries on from the answer's `next` until an answer holds none.
pub const PAGE: usize = 512;

/// Some of a history: the answer to `GET /v1/minds/{name}/history`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct History<'a> {
    /// The items from the cursor asked for.
    pub items: &'a [Item],
    /// The cursor to read on from.
    pub next: u64,
}

impl<'a> History<'a> {
    /// The items a history holds from index `after`.
    pub fn from(after: u64, items: &'a [Item]) -> Self {
        let read = u64::try_from(items.len()).unwrap_or(u64::MAX);
        Self {
            items,
            next: after.saturating_add(read),
        }
    }
}

/// Text a model has written of a reply that is not stored yet: the data of
/// a `writing` event, which `GET /v1/minds/{name}/events` sends between the
/// items of a history, with no id.
///
/// A reply's text comes in pieces, in order. A piece at 0 starts a reply,
/// or starts it over when the model had to be asked again. The item that is
/// stored when the turn moves on, a step as a rule, replaces all of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Writing<'a> {
    /// How many bytes of the reply's text came before this piece.
    pub at: u64,
    /// The piece.
    pub text: &'a str,
}

impl Writing<'_> {
    /// The name of the event.
    pub const EVENT: &'static str = "writing";
}

/// Whether a request's `Authorization` header carries `token` as a bearer
/// token. The comparison takes time that depends only on the lengths.
pub fn authorized(token: &str, authorization: Option<&str>) -> bool {
    let given = authorization.and_then(|value| value.strip_prefix("Bearer "));
    given.is_some_and(|given| {
        let (given, token) = (given.as_bytes(), token.as_bytes());
        let differs = given
            .iter()
            .zip(token)
            .fold(0, |differs, (a, b)| differs | (a ^ b));
        given.len() == token.len() && differs == 0
    })
}

#[cfg(test)]
mod tests {
    use mensor_mind::Origin;
    use serde_json::json;

    use super::*;

    #[test]
    fn answers_are_written_as_a_client_reads_them() {
        let status = MindStatus {
            mind: MindRecord {
                name: Name::new("m").unwrap(),
                origin: Origin::Root,
            },
            state: State::Working,
            hands: vec![Name::new("a1").unwrap()],
            waiting: vec![Name::new("b2").unwrap()],
            watching: vec![Name::new("m.k3x9q2ab").unwrap()],
            length: 3,
        };
        assert_eq!(
            serde_json::to_value(status).unwrap(),
            json!({"name": "m", "origin": "root", "state": "working", "hands": ["a1"],
                   "waiting": ["b2"], "watching": ["m.k3x9q2ab"], "length": 3})
        );
        let reply = PromptReply {
            status: Prompted::Accepted,
        };
        assert_eq!(
            serde_json::to_value(reply).unwrap(),
            json!({"status": "accepted"})
        );
        assert_eq!(
            (Prompted::Accepted.status(), Prompted::Existing.status()),
            (202, 200)
        );
        assert_eq!(
            serde_json::to_value(Problem::new(404, "no mind is named m")).unwrap(),
            json!({"status": 404, "detail": "no mind is named m"})
        );
        let writing = Writing {
            at: 5,
            text: " there",
        };
        assert_eq!(
            serde_json::to_value(writing).unwrap(),
            json!({"at": 5, "text": " there"})
        );
    }

    #[test]
    fn a_history_says_where_to_read_on_from() {
        let items = [Item::Paused {
            reason: "forked".into(),
        }];
        let history = History::from(7, &items);
        assert_eq!(
            serde_json::to_value(history).unwrap(),
            json!({"items": [{"type": "paused", "reason": "forked"}], "next": 8})
        );
        assert_eq!(History::from(9, &[]).next, 9);
    }

    #[test]
    fn a_prompt_may_carry_a_key() {
        let keyed: PromptBody = serde_json::from_value(json!({"key": "k", "text": "hi"})).unwrap();
        assert_eq!(keyed.key.as_deref(), Some("k"));
        let plain: PromptBody = serde_json::from_value(json!({"text": "hi"})).unwrap();
        assert_eq!(plain.key, None);
        assert!(serde_json::from_value::<PromptBody>(json!({"key": "k"})).is_err());
    }

    #[test]
    fn only_the_token_itself_is_authorized() {
        assert!(authorized("s3cret", Some("Bearer s3cret")));
        for other in [
            Some("Bearer s3cres"),
            Some("Bearer s3cret1"),
            Some("s3cret"),
            Some("Bearer "),
            None,
        ] {
            assert!(!authorized("s3cret", other), "{other:?}");
        }
    }
}

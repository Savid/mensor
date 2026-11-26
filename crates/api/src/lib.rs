//! The HTTP API's shapes, which every host of minds serves alike.
//!
//! `mensor serve` and the Cloudflare worker answer the same requests under
//! `/v1/minds` with the same JSON, so a client reads either the same way.
//! These are the bodies of those requests and answers, and who may ask:
//! [`access`]. How a request is routed and how an answer is sent stay with
//! each host.
//!
//! This crate is pure: no I/O, no clock and no randomness.

pub mod access;

pub use access::{Access, NotRoot, Token, authorized, local, mind_token};
use mensor_mind::{Item, MadeFrom, MindRecord, MindSettings, State, Switch, Trim};
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
#[serde(deny_unknown_fields)]
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

/// The body of `PUT /v1/minds/{name}`, which makes a mind: its settings,
/// each the host's when left out, and the history it starts from.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MakeBody {
    /// The model it asks: one the host offers.
    #[serde(default)]
    pub model: Option<String>,
    /// How hard that model works, in its API's words.
    #[serde(default)]
    pub effort: Option<String>,
    /// The system prompt it gives its model.
    #[serde(default)]
    pub system: Option<String>,
    /// When it leaves older results out of its requests.
    #[serde(default)]
    pub trim: Option<Trim>,
    /// Another mind's history to start from: the first `at` items of
    /// `mind`'s, keeping its hand `hand`. Without it, the mind starts from
    /// nothing. Each setting the body leaves out is then that mind's.
    #[serde(default)]
    pub from: Option<MadeFrom>,
}

impl MakeBody {
    /// The settings it names.
    pub fn settings(&self) -> MindSettings {
        MindSettings {
            model: self.model.clone(),
            effort: self.effort.clone(),
            system: self.system.clone(),
            trim: self.trim,
        }
    }
}

/// What became of a mind asked to be made.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Made {
    /// It is made.
    Made,
    /// It was made before, the same way.
    Existing,
}

impl Made {
    /// The HTTP status the answer is sent with.
    pub const fn status(self) -> u16 {
        match self {
            Self::Made => 201,
            Self::Existing => 200,
        }
    }
}

/// A mind and what it is doing: the answer to `GET /v1/minds/{name}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MindStatus {
    /// The mind, where its history begins, and the settings it was made
    /// with.
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
    /// The model it asks, and how hard, when a switch changed it from the
    /// one it was made with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub switched: Option<Switch>,
}

/// The body of `PUT /v1/minds/{name}/model`, which has a mind ask another
/// model from its next request on.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwitchBody {
    /// The model it asks: one the host offers.
    pub model: String,
    /// How hard that model works, in its API's words: the host's when left
    /// out, as for a mind made with that model.
    #[serde(default)]
    pub effort: Option<String>,
}

/// The body of `POST /v1/minds/{name}/trim`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrimBody {
    /// How many of the newest results stay whole: the `keep` of the mind's
    /// own trim, or none, when left out.
    #[serde(default)]
    pub keep: Option<u64>,
}

/// The body of `POST /v1/minds/{name}/release`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
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

#[cfg(test)]
mod tests {
    use mensor_mind::Origin;
    use serde_json::json;

    use super::*;

    #[test]
    fn answers_are_written_as_a_client_reads_them() {
        let status = MindStatus {
            mind: MindRecord::root(Name::new("m").unwrap()),
            state: State::Working,
            hands: vec![Name::new("a1").unwrap()],
            waiting: vec![Name::new("b2").unwrap()],
            watching: vec![Name::new("m.d3342iuj").unwrap()],
            length: 3,
            switched: None,
        };
        assert_eq!(
            serde_json::to_value(status).unwrap(),
            json!({"name": "m", "origin": "root", "state": "working", "hands": ["a1"],
                   "waiting": ["b2"], "watching": ["m.d3342iuj"], "length": 3})
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
    fn a_mind_is_made_with_settings_and_maybe_from_another() {
        let body: MakeBody = serde_json::from_value(json!({
            "model": "claude-haiku-5-5", "system": "Play.",
            "from": {"mind": "k", "at": 41, "hand": "vm"},
        }))
        .unwrap();
        assert_eq!(body.settings().model.as_deref(), Some("claude-haiku-5-5"));
        assert_eq!(body.settings().effort, None);
        let from = body.from.unwrap();
        assert_eq!(
            (from.mind.as_str(), from.at, from.hand.as_str()),
            ("k", 41, "vm")
        );
        let plain: MakeBody = serde_json::from_value(json!({})).unwrap();
        assert_eq!(plain.settings(), MindSettings::default());
        let trimming: MakeBody =
            serde_json::from_value(json!({"trim": {"keep": 2, "at": 60000}})).unwrap();
        assert_eq!(
            trimming.settings().trim,
            Some(Trim {
                keep: 2,
                at: Some(60_000)
            })
        );
        assert!(serde_json::from_value::<MakeBody>(json!({"trim": {"at": 60000}})).is_err());
        assert!(
            serde_json::from_value::<MakeBody>(json!({"trim": {"keep": 2, "after": 1}})).is_err()
        );
        // A word misspelt is refused, not left out.
        assert!(serde_json::from_value::<MakeBody>(json!({"modle": "x"})).is_err());
        assert!(
            serde_json::from_value::<MakeBody>(json!({"from": {"mind": "k", "at": 1}})).is_err()
        );
        assert_eq!((Made::Made.status(), Made::Existing.status()), (201, 200));

        let made = MindStatus {
            mind: MindRecord {
                name: Name::new("r2").unwrap(),
                origin: Origin::From {
                    from: MadeFrom {
                        mind: Name::new("k").unwrap(),
                        at: 41,
                        hand: Name::new("vm").unwrap(),
                    },
                },
                settings: std::sync::Arc::new(MindSettings {
                    model: Some("claude-haiku-5-5".into()),
                    ..MindSettings::default()
                }),
            },
            state: State::Ended,
            hands: Vec::new(),
            waiting: Vec::new(),
            watching: Vec::new(),
            length: 44,
            switched: None,
        };
        assert_eq!(
            serde_json::to_value(&made).unwrap(),
            json!({"name": "r2", "origin": "from", "from": {"mind": "k", "at": 41, "hand": "vm"},
                   "model": "claude-haiku-5-5", "state": "ended", "hands": [], "waiting": [],
                   "watching": [], "length": 44})
        );
        // A switched mind says what it was made with, and what it asks now.
        let switched = MindStatus {
            switched: Some(Switch {
                model: "openai-codex/gpt-6.1-terra".into(),
                effort: Some("high".into()),
            }),
            ..made
        };
        let written = serde_json::to_value(switched).unwrap();
        assert_eq!(written["model"], "claude-haiku-5-5");
        assert_eq!(
            written["switched"],
            json!({"model": "openai-codex/gpt-6.1-terra", "effort": "high"})
        );
    }

    #[test]
    fn a_switch_names_a_model_and_may_name_an_effort() {
        let body: SwitchBody =
            serde_json::from_value(json!({"model": "openai-codex/gpt-6.2-sol", "effort": "high"}))
                .unwrap();
        assert_eq!(
            (body.model.as_str(), body.effort.as_deref()),
            ("openai-codex/gpt-6.2-sol", Some("high"))
        );
        let plain: SwitchBody =
            serde_json::from_value(json!({"model": "claude-haiku-5-5"})).unwrap();
        assert_eq!(plain.effort, None);
        assert!(serde_json::from_value::<SwitchBody>(json!({"effort": "high"})).is_err());
        assert!(serde_json::from_value::<SwitchBody>(json!({"modle": "x"})).is_err());
    }

    #[test]
    fn a_prompt_may_carry_a_key() {
        let keyed: PromptBody = serde_json::from_value(json!({"key": "k", "text": "hi"})).unwrap();
        assert_eq!(keyed.key.as_deref(), Some("k"));
        let plain: PromptBody = serde_json::from_value(json!({"text": "hi"})).unwrap();
        assert_eq!(plain.key, None);
        assert!(serde_json::from_value::<PromptBody>(json!({"key": "k"})).is_err());
        let misspelt = json!({"text": "hi", "kye": "k"});
        assert!(serde_json::from_value::<PromptBody>(misspelt).is_err());
        let trim: TrimBody = serde_json::from_value(json!({"keep": 3})).unwrap();
        assert_eq!(trim.keep, Some(3));
        let trim: TrimBody = serde_json::from_value(json!({})).unwrap();
        assert_eq!(trim.keep, None);
        let released: ReleaseBody = serde_json::from_value(json!({"hand": "a1"})).unwrap();
        assert_eq!(released.hand.as_str(), "a1");
        let misspelt = json!({"hand": "a1", "why": "done"});
        assert!(serde_json::from_value::<ReleaseBody>(misspelt).is_err());
    }
}

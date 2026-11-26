use std::collections::BTreeMap;

use mensor_protocol::{CallId, CapabilityDecl, Name, Reply};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One entry in a mind's history.
///
/// The history only grows. An item's position in it is its index, which is
/// also the cursor a reader resumes from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum Item {
    /// Something said to the mind.
    Prompt {
        /// The caller's key. A prompt under a key the history already holds
        /// is that prompt, not a new one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
        /// What was said.
        text: String,
        /// The mind that said it: this mind's parent or one of its
        /// children. A prompt from anyone else has none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<Name>,
    },
    /// One reply from the model.
    Step {
        /// How many items the history held when the model was asked. A prompt
        /// at or past this index arrived too late for this step, and joins
        /// the next.
        seen: u64,
        /// What the model said and asked for.
        content: Vec<Block>,
        /// Why the model stopped.
        stop: Stop,
        /// What the step cost.
        #[serde(default)]
        usage: Usage,
        /// The model that answered, when the API named it: a request one
        /// model declined may be answered by another.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// What the model was asked under: a fingerprint of the model, the
        /// system prompt and the tools. The step's reasoning goes back to
        /// the model only under the same.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        under: Option<String>,
    },
    /// A call to a hand, stored before it is sent.
    Call {
        /// The call's id, as the hand knows it.
        id: CallId,
        /// The hand it is for.
        hand: Name,
        /// The model's tool use this call carries out.
        tool_use: String,
        /// The capability asked.
        capability: String,
        /// The operation of that capability.
        operation: String,
        /// The operation's input, without the `hand` the model named.
        input: Value,
    },
    /// How one of the model's tool uses ended.
    Outcome {
        /// The tool use this answers.
        tool_use: String,
        /// The call that carried it out, when one was made.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call: Option<CallId>,
        /// What happened.
        outcome: Outcome,
    },
    /// A hand said hello as a process, or with capabilities, the mind had
    /// not recorded: a new hand, one that restarted, or one that can now do
    /// something else.
    Attached {
        /// The hand.
        hand: Name,
        /// That process's boot.
        boot: String,
        /// What it can do.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        capabilities: Vec<CapabilityDecl>,
    },
    /// A clone of a hand was made into this mind. It is the mind's hand
    /// from now on, able to do what its original could, and joins when it
    /// says hello.
    Cloned {
        /// The clone.
        hand: Name,
        /// The hand it is a clone of.
        from: Name,
    },
    /// A hand is no longer the mind's. Its calls are settled, and it is
    /// refused if it says hello again.
    Released {
        /// The hand.
        hand: Name,
        /// Why, as the model is told it.
        reason: String,
    },
    /// This mind forked: the children its fork made, or may have made
    /// when the clone's outcome is unknown, and how many more minds it may
    /// fork.
    Forked {
        /// The children, in the order of the fork's prompts.
        children: Vec<Name>,
        /// How many more minds this mind may fork, or none for no limit.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        left: Option<u64>,
    },
    /// This mind was forked from `parent`: the first item of its own, after
    /// the outcome of the fork that made it.
    Parted {
        /// The mind it was forked from.
        parent: Name,
        /// How many minds it may fork, or none for no limit.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        left: Option<u64>,
    },
    /// This mind was made from the first `at` items of `from`'s history:
    /// the first item of its own after the hands it did not keep were let
    /// go. It is no kin of `from`, and starts with no parent, no children
    /// and a fork budget of its own.
    Branched {
        /// The mind whose items it shares.
        from: Name,
        /// How many of them.
        at: u64,
        /// How many minds it may fork, or none for no limit.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        left: Option<u64>,
        /// The highest id of a call `from` had made when this mind was
        /// made. This mind's own calls are numbered above it.
        last_call: CallId,
    },
    /// The mind ended: it let every hand go, asks its model no more, and
    /// takes no prompt. Its history stays, for the minds that share it.
    Ended,
    /// The mind stopped without asking its model, and waits for a prompt.
    Paused {
        /// Why.
        reason: String,
    },
    /// The model could not be asked, and the mind waits for a prompt.
    Failed {
        /// What went wrong.
        error: String,
    },
    /// Older results were left out to keep requests short: from here on,
    /// each result of an outcome before `before` is shown to the model as
    /// a one-line note. The steps, and what the model said in them, stay.
    Trimmed {
        /// The index of the first outcome whose result is still shown.
        before: u64,
    },
    /// The mind asks another model from here on: every request after this
    /// item, a summary's included, goes to that model at that effort. A
    /// fork's children, and a mind made from this history after it, ask it
    /// too, unless made with a model of their own.
    Switched(Switch),
    /// The conversation so far, as the model summed it up once it had grown
    /// past what the mind lets one request hold. The model is shown this in
    /// place of everything the steps before it had seen.
    ///
    /// A summary is also where a mind can start from: it carries what a
    /// mind needs of the history before it, so a mind holds, and a host
    /// loads, only the items from the index it names.
    Summary {
        /// The summary.
        text: String,
        /// What writing it cost.
        #[serde(default)]
        usage: Usage,
        /// The model that wrote it, when the API named it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// The index a mind starts from: that of the first item the steps
        /// before the summary had not seen.
        from: u64,
        /// Every hand the history had recorded, released ones included.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        hands: BTreeMap<Name, HandRecord>,
        /// The id of the last call the history holds, or 0.
        last_call: CallId,
        /// How many more minds the mind may fork, or none for no limit.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        forks_left: Option<u64>,
        /// The mind this one was forked from.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<Name>,
        /// Every mind this one's forks made.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        children: Vec<Name>,
        /// The model the mind asks, when a switch before the summary
        /// changed it from the one it was made with.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        switched: Option<Switch>,
    },
}

/// A model a mind was switched to, and how hard it works: see
/// [`Item::Switched`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Switch {
    /// The model, as a mind is made with it.
    pub model: String,
    /// How hard it works, in its API's words; without one, as hard as its
    /// API works by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

/// What a history records of one hand.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandRecord {
    /// The hand process last recorded.
    pub boot: String,
    /// What it can do.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<CapabilityDecl>,
    /// Whether the mind released it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub released: bool,
    /// Whether the mind made it with `clone_hand`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cloned: bool,
}

impl Item {
    /// The item's type, as it is written: `prompt`, `step` and so on.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Prompt { .. } => "prompt",
            Self::Step { .. } => "step",
            Self::Call { .. } => "call",
            Self::Outcome { .. } => "outcome",
            Self::Attached { .. } => "attached",
            Self::Cloned { .. } => "cloned",
            Self::Released { .. } => "released",
            Self::Forked { .. } => "forked",
            Self::Parted { .. } => "parted",
            Self::Branched { .. } => "branched",
            Self::Ended => "ended",
            Self::Paused { .. } => "paused",
            Self::Failed { .. } => "failed",
            Self::Trimmed { .. } => "trimmed",
            Self::Switched { .. } => "switched",
            Self::Summary { .. } => "summary",
        }
    }

    /// The key of a prompt that has one.
    pub fn key(&self) -> Option<&str> {
        match self {
            Self::Prompt { key, .. } => key.as_deref(),
            _ => None,
        }
    }
}

/// One part of a model's reply.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    /// Text.
    Text {
        /// The text.
        text: String,
    },
    /// A request to use a tool.
    ToolUse {
        /// The model's id for this use.
        id: String,
        /// The tool's name.
        name: String,
        /// The tool's input.
        input: Value,
    },
    /// A block the mind does not read, and hands back to the model exactly
    /// as it came: a model's signed reasoning, for one.
    Other {
        /// The block, as the model's provider wrote it.
        value: Value,
    },
}

/// Why a model stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stop {
    /// It finished what it had to say.
    EndTurn,
    /// It asked for tools.
    ToolUse,
    /// It ran out of output.
    MaxTokens,
    /// It declined.
    Refusal,
    /// Any other reason.
    Other,
}

/// How a tool use ended.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The hand ran it and returned this.
    Output(Value),
    /// The hand ran it, or tried to, and it failed.
    Error(String),
    /// Nothing ran it, for this reason.
    NotRun(String),
    /// The hand may have run it, but no result exists, for this reason.
    Unknown(String),
}

impl From<Reply> for Outcome {
    fn from(reply: Reply) -> Self {
        match reply {
            Reply::Output(output) => Self::Output(output),
            Reply::Error(error) => Self::Error(error),
        }
    }
}

/// What one step cost: in tokens, each kind priced on its own, and in time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    /// Tokens neither read from nor written to the cache.
    #[serde(default)]
    pub input: u64,
    /// Tokens read from the cache.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cache_read: u64,
    /// Tokens read and stored in the cache for later.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cache_written: u64,
    /// Tokens written.
    #[serde(default)]
    pub output: u64,
    /// Milliseconds from sending the request that was answered to the
    /// first of the reply in its streamed answer: when the model began to
    /// answer, reasoning included. Absent for an answer that did not
    /// stream, and where the host measures nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_token_ms: Option<u64>,
    /// Milliseconds from sending the request that was answered to the end
    /// of its answer, so that less `firstTokenMs` is the time the model
    /// spent writing. Failed attempts before it are not counted. Absent
    /// where the host measures nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub took_ms: Option<u64>,
}

impl Usage {
    /// Every token the step read, cached or not.
    pub fn read(&self) -> u64 {
        self.input + self.cache_read + self.cache_written
    }
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde passes a reference"
)]
fn is_zero(tokens: &u64) -> bool {
    *tokens == 0
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn items_are_written_as_a_reader_expects() {
        let cases = [
            (
                Item::Prompt {
                    key: None,
                    text: "hi".into(),
                    from: None,
                },
                json!({"type": "prompt", "text": "hi"}),
            ),
            (
                Item::Prompt {
                    key: Some("m/3/tu_1".into()),
                    text: "and the tests?".into(),
                    from: Some(Name::new("m").unwrap()),
                },
                json!({"type": "prompt", "key": "m/3/tu_1", "text": "and the tests?", "from": "m"}),
            ),
            (
                Item::Call {
                    id: 3,
                    hand: Name::new("a1").unwrap(),
                    tool_use: "tu_1".into(),
                    capability: "commands".into(),
                    operation: "bash".into(),
                    input: json!({"command": "ls"}),
                },
                json!({"type": "call", "id": 3, "hand": "a1", "toolUse": "tu_1", "capability": "commands",
                       "operation": "bash", "input": {"command": "ls"}}),
            ),
            (
                Item::Outcome {
                    tool_use: "tu_1".into(),
                    call: Some(3),
                    outcome: Outcome::Unknown("the hand restarted".into()),
                },
                json!({"type": "outcome", "toolUse": "tu_1", "call": 3,
                       "outcome": {"unknown": "the hand restarted"}}),
            ),
            (
                Item::Step {
                    seen: 1,
                    content: vec![Block::Text { text: "ok".into() }],
                    stop: Stop::EndTurn,
                    usage: Usage {
                        input: 10,
                        cache_read: 90,
                        cache_written: 5,
                        output: 2,
                        first_token_ms: Some(800),
                        took_ms: Some(2100),
                    },
                    model: Some("claude-sonnet-5-5".into()),
                    under: Some("0123456789abcdef".into()),
                },
                json!({"type": "step", "seen": 1, "content": [{"type": "text", "text": "ok"}],
                       "stop": "end_turn",
                       "usage": {"input": 10, "cacheRead": 90, "cacheWritten": 5, "output": 2,
                                 "firstTokenMs": 800, "tookMs": 2100},
                       "model": "claude-sonnet-5-5", "under": "0123456789abcdef"}),
            ),
        ];
        round_trip(cases);
    }

    #[test]
    fn what_a_mind_knows_of_itself_is_written_as_a_reader_expects() {
        let cases = [
            (
                Item::Summary {
                    text: "So far.".into(),
                    usage: Usage {
                        input: 10,
                        output: 2,
                        ..Usage::default()
                    },
                    model: None,
                    from: 4,
                    hands: BTreeMap::from([(
                        Name::new("a1").unwrap(),
                        HandRecord {
                            boot: "9f3c".into(),
                            capabilities: Vec::new(),
                            released: false,
                            cloned: false,
                        },
                    )]),
                    last_call: 3,
                    forks_left: Some(20),
                    parent: Some(Name::new("m").unwrap()),
                    children: vec![Name::new("m.d3342iuj").unwrap()],
                    switched: None,
                },
                json!({"type": "summary", "text": "So far.", "usage": {"input": 10, "output": 2},
                       "from": 4, "hands": {"a1": {"boot": "9f3c"}}, "lastCall": 3, "forksLeft": 20,
                       "parent": "m", "children": ["m.d3342iuj"]}),
            ),
            (
                Item::Released {
                    hand: Name::new("b2").unwrap(),
                    reason: "it stayed with m".into(),
                },
                json!({"type": "released", "hand": "b2", "reason": "it stayed with m"}),
            ),
            (
                Item::Forked {
                    children: vec![Name::new("m.d3342iuj").unwrap()],
                    left: Some(31),
                },
                json!({"type": "forked", "children": ["m.d3342iuj"], "left": 31}),
            ),
            (
                Item::Parted {
                    parent: Name::new("m").unwrap(),
                    left: Some(31),
                },
                json!({"type": "parted", "parent": "m", "left": 31}),
            ),
            // A mind with no limit to its forks says nothing of one.
            (
                Item::Parted {
                    parent: Name::new("m").unwrap(),
                    left: None,
                },
                json!({"type": "parted", "parent": "m"}),
            ),
            (
                Item::Summary {
                    text: "So far.".into(),
                    usage: Usage::default(),
                    model: None,
                    from: 0,
                    hands: BTreeMap::new(),
                    last_call: 0,
                    forks_left: None,
                    parent: None,
                    children: Vec::new(),
                    switched: None,
                },
                json!({"type": "summary", "text": "So far.", "usage": {"input": 0, "output": 0},
                       "from": 0, "lastCall": 0}),
            ),
            (
                Item::Branched {
                    from: Name::new("k").unwrap(),
                    at: 40,
                    left: Some(64),
                    last_call: 17,
                },
                json!({"type": "branched", "from": "k", "at": 40, "left": 64, "lastCall": 17}),
            ),
            (
                Item::Trimmed { before: 52 },
                json!({"type": "trimmed", "before": 52}),
            ),
            (
                Item::Switched(Switch {
                    model: "openai-codex/gpt-6.1-terra".into(),
                    effort: Some("high".into()),
                }),
                json!({"type": "switched", "model": "openai-codex/gpt-6.1-terra", "effort": "high"}),
            ),
            (Item::Ended, json!({"type": "ended"})),
        ];
        round_trip(cases);
    }

    fn round_trip<const N: usize>(cases: [(Item, Value); N]) {
        for (item, written) in cases {
            assert_eq!(serde_json::to_value(&item).unwrap(), written);
            assert_eq!(written["type"], item.kind());
            assert_eq!(serde_json::from_value::<Item>(written).unwrap(), item);
        }
    }
}

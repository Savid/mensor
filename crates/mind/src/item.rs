use mensor_protocol::{CallId, Reply};
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
    },
    /// A call to the body, stored before it is sent.
    Call {
        /// The call's id, as the body knows it.
        id: CallId,
        /// The model's tool use this call carries out.
        tool_use: String,
        /// The capability asked.
        capability: String,
        /// The operation of that capability.
        operation: String,
        /// The operation's input.
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
    /// A body process the mind had not recorded said hello.
    Attached {
        /// That process's boot.
        boot: String,
    },
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
        /// The index a mind starts from: that of the first item the steps
        /// before the summary had not seen.
        from: u64,
        /// The body process the history last recorded, if it had one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        boot: Option<String>,
        /// The id of the last call the history holds, or 0.
        last_call: CallId,
    },
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
            Self::Paused { .. } => "paused",
            Self::Failed { .. } => "failed",
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
    /// The body ran it and returned this.
    Output(Value),
    /// The body ran it, or tried to, and it failed.
    Error(String),
    /// Nothing ran it, for this reason.
    NotRun(String),
    /// The body may have run it, but no result exists, for this reason.
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

/// What one step cost, in tokens.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Tokens read.
    #[serde(default)]
    pub input: u64,
    /// Tokens written.
    #[serde(default)]
    pub output: u64,
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
                },
                json!({"type": "prompt", "text": "hi"}),
            ),
            (
                Item::Call {
                    id: 3,
                    tool_use: "tu_1".into(),
                    capability: "commands".into(),
                    operation: "bash".into(),
                    input: json!({"command": "ls"}),
                },
                json!({"type": "call", "id": 3, "toolUse": "tu_1", "capability": "commands",
                       "operation": "bash", "input": {"command": "ls"}}),
            ),
            (
                Item::Outcome {
                    tool_use: "tu_1".into(),
                    call: Some(3),
                    outcome: Outcome::Unknown("the body restarted".into()),
                },
                json!({"type": "outcome", "toolUse": "tu_1", "call": 3,
                       "outcome": {"unknown": "the body restarted"}}),
            ),
            (
                Item::Step {
                    seen: 1,
                    content: vec![Block::Text { text: "ok".into() }],
                    stop: Stop::EndTurn,
                    usage: Usage {
                        input: 10,
                        output: 2,
                    },
                },
                json!({"type": "step", "seen": 1, "content": [{"type": "text", "text": "ok"}],
                       "stop": "end_turn", "usage": {"input": 10, "output": 2}}),
            ),
            (
                Item::Summary {
                    text: "So far.".into(),
                    usage: Usage {
                        input: 10,
                        output: 2,
                    },
                    from: 4,
                    boot: Some("9f3c".into()),
                    last_call: 3,
                },
                json!({"type": "summary", "text": "So far.", "usage": {"input": 10, "output": 2},
                       "from": 4, "boot": "9f3c", "lastCall": 3}),
            ),
        ];
        for (item, written) in cases {
            assert_eq!(serde_json::to_value(&item).unwrap(), written);
            assert_eq!(written["type"], item.kind());
            assert_eq!(serde_json::from_value::<Item>(written).unwrap(), item);
        }
    }
}

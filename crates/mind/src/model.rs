use serde_json::Value;

use crate::{Block, Stop, Usage};

/// What a mind asks its model.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelRequest {
    /// How many items the history held when this was built. The host hands it
    /// back with the reply.
    pub seen: u64,
    /// The system prompt.
    pub system: Option<String>,
    /// The conversation so far, user first, roles alternating.
    pub messages: Vec<Message>,
    /// The tools the model may use.
    pub tools: Vec<Tool>,
    /// Whether the model must answer in text. It is still shown the tools,
    /// so that the request extends the last one's prefix, but may use none.
    pub text_only: bool,
}

/// What a model answers.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelReply {
    /// What it said and asked for.
    pub content: Vec<Block>,
    /// Why it stopped.
    pub stop: Stop,
    /// What the reply cost.
    pub usage: Usage,
}

/// Why a model could not answer.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ModelError(pub String);

/// One message of a conversation.
#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    /// Who speaks.
    pub role: Role,
    /// What they say.
    pub content: Vec<Content>,
}

/// Who speaks a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// The mind's side: prompts and tool results.
    User,
    /// The model.
    Assistant,
}

/// One part of a message.
#[derive(Clone, Debug, PartialEq)]
pub enum Content {
    /// Text.
    Text(String),
    /// The model's request to use a tool.
    ToolUse {
        /// The model's id for this use.
        id: String,
        /// The tool's name.
        name: String,
        /// The tool's input.
        input: Value,
    },
    /// A block of the model's own, handed back exactly as it came.
    Other(Value),
    /// How a tool use ended.
    ToolResult {
        /// The tool use this answers.
        tool_use: String,
        /// What the model is told.
        text: String,
        /// Whether it is told as an error.
        error: bool,
    },
}

/// A tool the model is shown.
#[derive(Clone, Debug, PartialEq)]
pub struct Tool {
    /// The name the model uses.
    pub name: String,
    /// What the tool does, written for the model.
    pub description: String,
    /// The JSON Schema of the tool's input.
    pub input: Value,
}

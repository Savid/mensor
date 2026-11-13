use std::collections::HashMap;

use mensor_protocol::CapabilityDecl;
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

/// A tool the model is shown, and the capability operation that does its work.
#[derive(Clone, Debug, PartialEq)]
pub struct Tool {
    /// The name the model uses.
    pub name: String,
    /// What the tool does, written for the model.
    pub description: String,
    /// The JSON Schema of the tool's input.
    pub input: Value,
    /// The capability behind the tool.
    pub capability: String,
    /// The operation of that capability.
    pub operation: String,
}

/// The tools a body's capabilities give the model.
///
/// A tool is named after its operation, such as `bash` or `read`. When two
/// capabilities both have an operation of one name, each of those tools is
/// named `<capability>_<operation>`.
pub fn tools(capabilities: &[CapabilityDecl]) -> Vec<Tool> {
    let mut uses: HashMap<&str, usize> = HashMap::new();
    for operation in capabilities
        .iter()
        .flat_map(|capability| &capability.operations)
    {
        *uses.entry(&operation.name).or_default() += 1;
    }
    capabilities
        .iter()
        .flat_map(|capability| {
            let uses = &uses;
            capability.operations.iter().map(move |operation| Tool {
                name: if uses[operation.name.as_str()] > 1 {
                    format!("{}_{}", capability.name, operation.name)
                } else {
                    operation.name.clone()
                },
                description: operation.description.clone(),
                input: operation.input.clone(),
                capability: capability.name.clone(),
                operation: operation.name.clone(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use mensor_protocol::OperationDecl;
    use serde_json::json;

    use super::*;

    fn capability(name: &str, operations: &[&str]) -> CapabilityDecl {
        CapabilityDecl {
            name: name.into(),
            operations: operations
                .iter()
                .map(|operation| OperationDecl {
                    name: (*operation).into(),
                    description: format!("{name} {operation}"),
                    input: json!({"type": "object"}),
                })
                .collect(),
        }
    }

    #[test]
    fn a_tool_is_named_after_its_operation() {
        let tools = tools(&[
            capability("commands", &["bash"]),
            capability("files", &["read"]),
        ]);
        let names: Vec<_> = tools.iter().map(|tool| tool.name.as_str()).collect();
        assert_eq!(names, ["bash", "read"]);
        assert_eq!(
            (tools[1].capability.as_str(), tools[1].operation.as_str()),
            ("files", "read")
        );
    }

    #[test]
    fn operations_that_share_a_name_carry_their_capability() {
        let tools = tools(&[
            capability("files", &["read", "write"]),
            capability("notes", &["read"]),
        ]);
        let names: Vec<_> = tools.iter().map(|tool| tool.name.as_str()).collect();
        assert_eq!(names, ["files_read", "write", "notes_read"]);
    }
}

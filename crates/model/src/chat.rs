//! Chat Completions: `POST /chat/completions`, as `OpenAI`, `OpenRouter` and
//! most other model servers speak it.

use std::borrow::Cow;

use mensor_mind::{Block, Content, Message, ModelReply, ModelRequest, Role, Stop, Usage};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::{CUT_OFF, Config, Credential, Post};

/// The type of the block a reply's reasoning details are kept in. A server
/// that signs its reasoning wants it back with the step it came with.
const REASONING: &str = "reasoning_details";

pub(crate) fn post(config: &Config, request: &ModelRequest) -> Post {
    let url = format!("{}/chat/completions", config.base_url.trim_end_matches('/'));
    let (Credential::ApiKey(key) | Credential::Token(key)) = &config.credential;
    let body = serde_json::to_string(&body(config, request))
        .expect("a request is strings and JSON values, which are always written");
    Post {
        url,
        headers: vec![("authorization", format!("Bearer {key}"))],
        body,
    }
}

/// Whether a block is reasoning this API wrote.
pub(crate) fn is_reasoning(block: &Value) -> bool {
    block.get("type").and_then(Value::as_str) == Some(REASONING)
}

/// The body of one request. It borrows the conversation.
#[derive(Serialize)]
struct Body<'a> {
    model: &'a str,
    stream: bool,
    stream_options: StreamOptions,
    messages: Vec<Said<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ToolDef<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'a str>,
}

/// A stream says what the reply cost only when it is asked to.
#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Serialize)]
struct ToolDef<'a> {
    r#type: &'static str,
    function: Function<'a>,
}

#[derive(Serialize)]
struct Function<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a Value,
}

#[derive(Default, Serialize)]
struct Said<'a> {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<ToolCall<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_details: Option<&'a Value>,
}

#[derive(Serialize)]
struct ToolCall<'a> {
    id: &'a str,
    r#type: &'static str,
    function: Called<'a>,
}

#[derive(Serialize)]
struct Called<'a> {
    name: &'a str,
    /// The input, as JSON in a string.
    arguments: String,
}

fn body<'a>(config: &'a Config, request: &'a ModelRequest) -> Body<'a> {
    let mut messages = Vec::with_capacity(request.messages.len() + 1);
    if let Some(system) = request
        .system
        .as_deref()
        .filter(|system| !system.is_empty())
    {
        messages.push(Said {
            role: "system",
            content: Some(system.to_owned()),
            ..Said::default()
        });
    }
    for message in &request.messages {
        match message.role {
            Role::User => user(&mut messages, message),
            Role::Assistant => messages.extend(assistant(message)),
        }
    }
    Body {
        model: request.model.as_deref().unwrap_or(&config.model),
        stream: true,
        stream_options: StreamOptions {
            include_usage: true,
        },
        messages,
        tools: request
            .tools
            .iter()
            .map(|tool| ToolDef {
                r#type: "function",
                function: Function {
                    name: &tool.name,
                    description: &tool.description,
                    parameters: &tool.input,
                },
            })
            .collect(),
        tool_choice: (request.text_only && !request.tools.is_empty()).then_some("none"),
        reasoning_effort: request.effort.as_deref().or(config.effort.as_deref()),
    }
}

/// A user message becomes one `tool` message for each result, which must
/// follow the step that asked, and then one `user` message of its text.
fn user<'a>(messages: &mut Vec<Said<'a>>, message: &'a Message) {
    let mut text = String::new();
    for content in &message.content {
        match content {
            Content::ToolResult {
                tool_use,
                text,
                error,
            } => messages.push(Said {
                role: "tool",
                content: Some(told(text, *error).into_owned()),
                tool_call_id: Some(tool_use),
                ..Said::default()
            }),
            Content::Text(said) if !said.is_empty() => {
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(said);
            }
            Content::Text(_) | Content::ToolUse { .. } | Content::Other(_) => {}
        }
    }
    if text.is_empty() {
        return;
    }
    // Some servers refuse two user messages in a row.
    match messages.last_mut() {
        Some(Said {
            role: "user",
            content: Some(last),
            ..
        }) => {
            last.push_str("\n\n");
            last.push_str(&text);
        }
        _ => messages.push(Said {
            role: "user",
            content: Some(text),
            ..Said::default()
        }),
    }
}

/// A tool's input as JSON in a string, as this API and the Responses API
/// take it: one that was not JSON goes back as the model wrote it.
pub(crate) fn arguments(input: &Value) -> String {
    match input {
        Value::String(written) => written.clone(),
        input => input.to_string(),
    }
}

/// A tool's result as this API and the Responses API are told it. Neither
/// has a flag for a failed call, so the text says.
pub(crate) fn told(text: &str, error: bool) -> Cow<'_, str> {
    match (text.is_empty(), error) {
        (true, _) => Cow::Borrowed("(no output)"),
        (false, true) => Cow::Owned(format!("Error: {text}")),
        (false, false) => Cow::Borrowed(text),
    }
}

fn assistant(message: &Message) -> Option<Said<'_>> {
    let mut said = Said {
        role: "assistant",
        ..Said::default()
    };
    let mut text = String::new();
    for content in &message.content {
        match content {
            Content::Text(part) if !part.is_empty() => {
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(part);
            }
            Content::ToolUse { id, name, input } => said.tool_calls.push(ToolCall {
                id,
                r#type: "function",
                function: Called {
                    name,
                    arguments: arguments(input),
                },
            }),
            // Reasoning another API wrote means nothing to this one.
            Content::Other(block) if is_reasoning(block) => {
                said.reasoning_details = block.get("details");
            }
            Content::Text(_) | Content::Other(_) | Content::ToolResult { .. } => {}
        }
    }
    if text.is_empty() && said.tool_calls.is_empty() {
        return None;
    }
    said.content = (!text.is_empty()).then_some(text);
    Some(said)
}

#[derive(Deserialize)]
struct Reply {
    /// The model that answered: behind a router, the one that served it.
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<ReplyUsage>,
}

#[derive(Deserialize)]
struct Choice {
    message: Answered,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct Answered {
    content: Option<String>,
    refusal: Option<String>,
    tool_calls: Option<Vec<Asked>>,
    reasoning_details: Option<Value>,
}

#[derive(Deserialize)]
struct Asked {
    id: String,
    function: AskedFunction,
}

#[derive(Deserialize)]
struct AskedFunction {
    name: String,
    #[serde(default)]
    arguments: String,
}

#[derive(Deserialize)]
struct ReplyUsage {
    /// Every token read, those of the cache included, as most servers count.
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<PromptDetails>,
    #[serde(default)]
    completion_tokens: u64,
}

/// Some servers write a count they do not keep as null.
#[derive(Default, Deserialize)]
struct PromptDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
    /// Written by a server that charges for writing the cache.
    #[serde(default)]
    cache_write_tokens: Option<u64>,
}

/// Reads one response.
pub(crate) fn reply(body: &str) -> Result<ModelReply, String> {
    let reply: Reply = serde_json::from_str(body).map_err(|error| error.to_string())?;
    let Some(Choice {
        message,
        finish_reason,
    }) = reply.choices.into_iter().next()
    else {
        return Err("it holds no choice".into());
    };

    let mut content = Vec::new();
    let reasoned = message.reasoning_details.filter(|details| match details {
        Value::Null => false,
        Value::Array(details) => !details.is_empty(),
        _ => true,
    });
    if let Some(details) = reasoned {
        content.push(Block::Other {
            value: json!({"type": REASONING, "details": details}),
        });
    }
    let refused = message.refusal.filter(|refusal| !refusal.is_empty());
    let text = message.content.filter(|text| !text.is_empty());
    if let Some(text) = text.or_else(|| refused.clone()) {
        content.push(Block::Text { text });
    }
    let calls = message.tool_calls.unwrap_or_default();
    let called = !calls.is_empty();
    content.extend(calls.into_iter().map(|call| Block::ToolUse {
        id: call.id,
        name: call.function.name,
        input: input(call.function.arguments),
    }));

    let stop = match finish_reason.as_deref() {
        Some("length") => Stop::MaxTokens,
        Some("content_filter") => Stop::Refusal,
        // Some servers end a reply that asks for tools with `stop`.
        _ if called => Stop::ToolUse,
        _ if refused.is_some() => Stop::Refusal,
        Some("stop") => Stop::EndTurn,
        _ => Stop::Other,
    };
    let usage = reply.usage.map_or_else(Usage::default, |usage| {
        let cache = usage.prompt_tokens_details.unwrap_or_default();
        let read = cache.cached_tokens.unwrap_or(0);
        let written = cache.cache_write_tokens.unwrap_or(0);
        Usage {
            // A server whose prompt count leaves the cache out has it all.
            input: usage
                .prompt_tokens
                .checked_sub(read + written)
                .unwrap_or(usage.prompt_tokens),
            cache_read: read,
            cache_written: written,
            output: usage.completion_tokens,
        }
    });
    Ok(ModelReply {
        model: reply.model,
        content,
        stop,
        usage,
    })
}

/// A tool's input, from the JSON a model wrote in a string. Nothing at all
/// is an empty object. What is not JSON is kept as the string it is, and
/// the capability it reaches says what is wrong with it.
pub(crate) fn input(arguments: String) -> Value {
    if arguments.trim().is_empty() {
        return Value::Object(Map::new());
    }
    serde_json::from_str(&arguments).unwrap_or(Value::String(arguments))
}

/// A streamed reply so far, put together as the completion a request that
/// is not streamed is answered with.
#[derive(Debug, Default)]
pub(crate) struct Parts {
    content: String,
    refusal: String,
    calls: Vec<Calling>,
    reasoning: Vec<Value>,
    finish_reason: Option<String>,
    usage: Option<Value>,
    ended: bool,
    error: Option<String>,
}

/// One tool call, as much of it as has come.
#[derive(Debug, Default, Serialize)]
struct Calling {
    id: String,
    function: CallingFunction,
}

#[derive(Debug, Default, Serialize)]
struct CallingFunction {
    name: String,
    arguments: String,
}

#[derive(Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<ChunkChoice>,
    usage: Option<Value>,
    error: Option<Value>,
}

#[derive(Deserialize)]
struct ChunkChoice {
    #[serde(default)]
    delta: ChunkDelta,
    finish_reason: Option<String>,
}

#[derive(Default, Deserialize)]
struct ChunkDelta {
    content: Option<String>,
    refusal: Option<String>,
    tool_calls: Option<Vec<CallPiece>>,
    reasoning_details: Option<Vec<Value>>,
}

#[derive(Deserialize)]
struct CallPiece {
    index: Option<usize>,
    id: Option<String>,
    function: Option<FunctionPiece>,
}

#[derive(Deserialize)]
struct FunctionPiece {
    name: Option<String>,
    arguments: Option<String>,
}

impl Parts {
    /// Takes one event. Text the model wrote in it joins `text`.
    pub(crate) fn take(&mut self, data: &str, text: &mut String) {
        if data == "[DONE]" {
            self.ended = true;
            return;
        }
        let chunk: Chunk = match serde_json::from_str(data) {
            Ok(chunk) => chunk,
            Err(error) => {
                self.error = Some(format!("an event of the stream could not be read: {error}"));
                return;
            }
        };
        if let Some(error) = chunk.error {
            self.error = Some(crate::said(&json!({"error": error}).to_string()));
            return;
        }
        if chunk.usage.is_some() {
            self.usage = chunk.usage;
        }
        let Some(choice) = chunk.choices.into_iter().next() else {
            return;
        };
        if let Some(piece) = choice.delta.content {
            text.push_str(&piece);
            self.content.push_str(&piece);
        }
        if let Some(piece) = choice.delta.refusal {
            self.refusal.push_str(&piece);
        }
        for piece in choice.delta.tool_calls.unwrap_or_default() {
            self.call(piece);
        }
        for piece in choice.delta.reasoning_details.unwrap_or_default() {
            match self.reasoning.last_mut() {
                Some(last) if continues(last, &piece) => extend(last, piece),
                _ => self.reasoning.push(piece),
            }
        }
        // Not every server ends its stream with `[DONE]`.
        if choice.finish_reason.is_some() {
            self.finish_reason = choice.finish_reason;
            self.ended = true;
        }
    }

    /// Adds a piece of a tool call to the call it belongs to: the one at
    /// its index, or else the last. A piece with a new id starts a call,
    /// whatever its index, as some servers number every call 0.
    fn call(&mut self, piece: CallPiece) {
        let id = piece.id.filter(|id| !id.is_empty());
        let last = self.calls.len().saturating_sub(1);
        let mut at = piece.index.unwrap_or(last);
        let other =
            |held: &Calling| !held.id.is_empty() && id.as_ref().is_some_and(|id| *id != held.id);
        if self.calls.get(at).is_some_and(other) {
            at = self.calls.len();
        }
        if self.calls.len() <= at {
            self.calls.resize_with(at + 1, Calling::default);
        }
        let call = &mut self.calls[at];
        if let Some(id) = id {
            call.id = id;
        }
        let Some(function) = piece.function else {
            return;
        };
        if let Some(name) = function.name.filter(|name| !name.is_empty()) {
            call.function.name = name;
        }
        if let Some(arguments) = function.arguments {
            call.function.arguments.push_str(&arguments);
        }
    }

    pub(crate) fn finish(self) -> Result<String, String> {
        if let Some(error) = self.error {
            return Err(error);
        }
        if !self.ended {
            return Err(CUT_OFF.into());
        }
        let message = json!({
            "content": self.content,
            "refusal": self.refusal,
            "tool_calls": self.calls,
            "reasoning_details": self.reasoning,
        });
        let choice = json!({"message": message, "finish_reason": self.finish_reason});
        Ok(json!({"choices": [choice], "usage": self.usage}).to_string())
    }
}

/// Whether a piece of reasoning carries on the detail before it: the two
/// are of one type and one index.
fn continues(last: &Value, piece: &Value) -> bool {
    last.get("type") == piece.get("type") && last.get("index") == piece.get("index")
}

/// Adds a piece of reasoning to the detail it carries on. What the detail
/// holds comes in pieces, and the rest of it once, sometimes at the end.
fn extend(last: &mut Value, piece: Value) {
    let (Value::Object(last), Value::Object(piece)) = (last, piece) else {
        return;
    };
    for (key, value) in piece {
        let held = matches!(key.as_str(), "text" | "summary" | "data");
        match (last.get_mut(&key), value) {
            (_, Value::Null) => {}
            (Some(Value::String(so_far)), Value::String(more)) if held => so_far.push_str(&more),
            (_, value) => {
                last.insert(key, value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{body_of, request};
    use crate::{Answer, Exchange, Next};

    fn config() -> Config {
        Config::chat("sk-test", "gpt")
    }

    #[test]
    fn a_request_is_written_as_the_api_reads_it() {
        let (config, mut request) = (config(), request());
        let post = Exchange::new(&config, &request).post();
        assert_eq!(post.url, "https://api.openai.com/v1/chat/completions");
        assert_eq!(post.header("authorization"), Some("Bearer sk-test"));
        assert_eq!(
            body_of(&post),
            json!({
                "model": "gpt",
                "stream": true,
                "stream_options": {"include_usage": true},
                "messages": [
                    {"role": "system", "content": "Be brief."},
                    {"role": "user", "content": "list the files"},
                    {"role": "assistant", "tool_calls": [{
                        "id": "tu_1", "type": "function",
                        "function": {"name": "bash", "arguments": r#"{"command":"ls"}"#},
                    }, {
                        "id": "tu_2", "type": "function",
                        "function": {"name": "bash", "arguments": r#"{"command":"cat x"}"#},
                    }]},
                    {"role": "tool", "tool_call_id": "tu_1", "content": "(no output)"},
                    {"role": "tool", "tool_call_id": "tu_2", "content": "Error: no"},
                ],
                "tools": [{"type": "function", "function": {
                    "name": "bash", "description": "Run a command.", "parameters": {"type": "object"},
                }}],
            })
        );

        // Arguments that were not JSON go back as the model wrote them.
        if let Content::ToolUse { input, .. } = &mut request.messages[1].content[3] {
            *input = json!("cat x");
        }
        let body = body_of(&Exchange::new(&config, &request).post());
        let called = &body["messages"][2]["tool_calls"][1]["function"]["arguments"];
        assert_eq!(called, &json!("cat x"));
    }

    #[test]
    fn a_mind_with_a_model_of_its_own_asks_it() {
        let mut config = config();
        config.effort = Some("high".into());
        let request = ModelRequest {
            model: Some("gpt-mini".into()),
            effort: Some("low".into()),
            ..request()
        };
        let body = body_of(&Exchange::new(&config, &request).post());
        assert_eq!(body["model"], json!("gpt-mini"));
        assert_eq!(body["reasoning_effort"], json!("low"));
    }

    #[test]
    fn a_request_for_text_alone_lets_no_tool_be_called() {
        let mut config = config();
        config.effort = Some("high".into());
        let request = ModelRequest {
            text_only: true,
            ..request()
        };
        let body = body_of(&Exchange::new(&config, &request).post());
        assert_eq!(body["tool_choice"], json!("none"));
        assert_eq!(body["reasoning_effort"], json!("high"));
    }

    #[test]
    fn user_messages_in_a_row_are_one() {
        let said = |text: &str| Message {
            role: Role::User,
            content: vec![Content::Text(text.into())],
        };
        let request = ModelRequest {
            system: None,
            messages: vec![
                said("one"),
                Message {
                    role: Role::Assistant,
                    content: vec![Content::Other(json!({"type": "thinking"}))],
                },
                said("two"),
            ],
            tools: Vec::new(),
            ..request()
        };
        let body = body_of(&Exchange::new(&config(), &request).post());
        assert_eq!(
            body["messages"],
            json!([{"role": "user", "content": "one\n\ntwo"}])
        );
    }

    #[test]
    fn a_reply_is_read_with_its_calls_and_its_reasoning() {
        let details = json!([{"type": "reasoning.encrypted", "data": "sig"}]);
        let body = json!({
            "model": "anthropic/claude-sonnet-5.5",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "Looking.",
                    "reasoning_details": details,
                    "tool_calls": [
                        {"id": "call_1", "type": "function",
                         "function": {"name": "bash", "arguments": r#"{"command":"ls"}"#}},
                        {"id": "call_2", "type": "function",
                         "function": {"name": "list", "arguments": ""}},
                        {"id": "call_3", "type": "function",
                         "function": {"name": "bash", "arguments": "{\"command\":"}},
                    ],
                },
                "finish_reason": "tool_calls",
            }],
            "usage": {"prompt_tokens": 100, "completion_tokens": 7,
                      "prompt_tokens_details": {"cached_tokens": 80, "cache_write_tokens": 15}},
        });
        let read = reply(&body.to_string()).unwrap();
        let reasoning = json!({"type": "reasoning_details", "details": details});
        assert_eq!(
            read.content,
            vec![
                Block::Other {
                    value: reasoning.clone()
                },
                Block::Text {
                    text: "Looking.".into()
                },
                Block::ToolUse {
                    id: "call_1".into(),
                    name: "bash".into(),
                    input: json!({"command": "ls"})
                },
                Block::ToolUse {
                    id: "call_2".into(),
                    name: "list".into(),
                    input: json!({})
                },
                Block::ToolUse {
                    id: "call_3".into(),
                    name: "bash".into(),
                    input: json!("{\"command\":")
                },
            ]
        );
        assert_eq!(read.stop, Stop::ToolUse);
        assert_eq!(read.model.as_deref(), Some("anthropic/claude-sonnet-5.5"));
        assert_eq!(
            read.usage,
            Usage {
                input: 5,
                cache_read: 80,
                cache_written: 15,
                output: 7
            }
        );
        // A server whose prompt count leaves the cache out, or that writes
        // a count it does not keep as null.
        let usage = |usage: Value| {
            let body = json!({"choices": [{"message": {"content": "hi"}, "finish_reason": "stop"}],
                              "usage": usage});
            reply(&body.to_string()).unwrap().usage
        };
        let uncounted = usage(json!({"prompt_tokens": 10, "completion_tokens": 1,
                                     "prompt_tokens_details": {"cached_tokens": 80}}));
        assert_eq!((uncounted.input, uncounted.cache_read), (10, 80));
        let unkept = usage(json!({"prompt_tokens": 10, "completion_tokens": 1,
                                  "prompt_tokens_details": {"cached_tokens": null}}));
        assert_eq!((unkept.input, unkept.cache_read), (10, 0));

        // The reasoning goes back with the step it came with.
        let request = ModelRequest {
            system: None,
            messages: vec![Message {
                role: Role::Assistant,
                content: vec![Content::Other(reasoning), Content::Text("Looking.".into())],
            }],
            tools: Vec::new(),
            ..request()
        };
        let sent = body_of(&Exchange::new(&config(), &request).post());
        assert_eq!(
            sent["messages"],
            json!([{"role": "assistant", "content": "Looking.", "reasoning_details": details}])
        );
    }

    #[test]
    fn every_finish_reason_has_a_meaning() {
        let stop = |message: Value, reason: Value| {
            let body = json!({"choices": [{"message": message, "finish_reason": reason}]});
            reply(&body.to_string()).unwrap().stop
        };
        let text = || json!({"content": "hi"});
        let call = || {
            json!({"content": null, "tool_calls": [
                {"id": "c", "function": {"name": "bash", "arguments": "{}"}}
            ]})
        };
        assert_eq!(stop(text(), json!("stop")), Stop::EndTurn);
        assert_eq!(stop(text(), json!("length")), Stop::MaxTokens);
        assert_eq!(stop(text(), json!("content_filter")), Stop::Refusal);
        assert_eq!(stop(text(), Value::Null), Stop::Other);
        assert_eq!(stop(call(), json!("tool_calls")), Stop::ToolUse);
        assert_eq!(stop(call(), json!("stop")), Stop::ToolUse);
        // A call cut off mid-way does not run.
        assert_eq!(stop(call(), json!("length")), Stop::MaxTokens);
        assert_eq!(
            stop(json!({"content": null, "refusal": "No."}), json!("stop")),
            Stop::Refusal
        );
    }

    #[test]
    fn a_reply_with_no_choice_ends_the_call() {
        let (config, request) = (config(), request());
        let mut exchange = Exchange::new(&config, &request);
        let empty = Answer::Response {
            status: 200,
            retry_after: None,
            request_id: None,
            body: json!({"choices": []}).to_string(),
        };
        assert!(matches!(exchange.answered(empty), Next::Done(Err(_))));
    }
}

//! Claude's Messages API: `POST /v1/messages`.

use std::sync::LazyLock;

use mensor_mind::{Block, Content, Message, ModelReply, ModelRequest, Role, Stop, Usage};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::{CUT_OFF, Config, Credential, Failure, Post};

/// The API version every request names.
pub const VERSION: &str = "2026-10-21";
/// Lets the API serve a declined request from another model in the same call.
pub const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
/// Lets a request say what happens to reasoning bound to an edited history.
pub const BINDING_BETA: &str = "thinking-binding-controls-2026-08-01";

pub(crate) fn post(config: &Config, request: &ModelRequest, drop_reasoning: bool) -> Post {
    let url = format!("{}/v1/messages", config.base_url.trim_end_matches('/'));
    let mut headers = vec![("anthropic-version", VERSION.to_owned())];
    let mut betas = Vec::new();
    match &config.credential {
        Credential::ApiKey(key) => headers.push(("x-api-key", key.clone())),
        Credential::Token(token) => headers.push(("authorization", format!("Bearer {token}"))),
    }
    if config.fallbacks {
        betas.push(FALLBACK_BETA);
    }
    if drop_reasoning {
        betas.push(BINDING_BETA);
    }
    if !betas.is_empty() {
        headers.push(("anthropic-beta", betas.join(",")));
    }
    let body = serde_json::to_string(&body(config, request, drop_reasoning))
        .expect("a request is strings and JSON values, which are always written");
    Post { url, headers, body }
}

/// Whether the API refused reasoning that the history no longer matches.
pub(crate) fn reasoning_is_unbound(failure: &Failure) -> bool {
    failure.status == Some(400) && failure.detail.contains("bound to a different conversation")
}

/// The body of one request. It borrows the conversation.
#[derive(Serialize)]
struct Body<'a> {
    model: &'a str,
    /// The API refuses a request without one.
    max_tokens: u32,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ToolDef<'a>>,
    messages: Vec<Said<'a>>,
    cache_control: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fallbacks: Option<&'static str>,
    thinking: Value,
}

#[derive(Serialize)]
struct ToolDef<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a Value,
}

#[derive(Serialize)]
struct Said<'a> {
    role: &'static str,
    content: Vec<Part<'a>>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Part<'a> {
    Text {
        text: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<&'static Value>,
    },
    ToolUse {
        id: &'a str,
        name: &'a str,
        input: &'a Value,
    },
    ToolResult {
        tool_use_id: &'a str,
        content: &'a str,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<&'static Value>,
    },
    /// A block of the model's own, written as it came.
    #[serde(untagged)]
    Other(&'a Value),
}

// A request that must be answered in text is not told so with `tool_choice`:
// that would stop it reading the cache the request before it wrote. The
// mind asks for text in words, and takes only the text of the reply.
fn body<'a>(config: &'a Config, request: &'a ModelRequest, drop_reasoning: bool) -> Body<'a> {
    Body {
        model: request.model.as_deref().unwrap_or(&config.model),
        max_tokens: config.max_tokens,
        stream: true,
        system: request
            .system
            .as_deref()
            .filter(|system| !system.is_empty()),
        tools: request
            .tools
            .iter()
            .map(|tool| ToolDef {
                name: &tool.name,
                description: &tool.description,
                input_schema: &tool.input,
            })
            .collect(),
        messages: conversation(&request.messages),
        // A history only grows, so each request extends the last one's
        // prefix, up to the thinking it leaves out.
        cache_control: BREAKPOINT.clone(),
        output_config: request
            .effort
            .as_ref()
            .or(config.effort.as_ref())
            .map(|effort| json!({"effort": effort})),
        fallbacks: config.fallbacks.then_some("default"),
        thinking: thinking(drop_reasoning),
    }
}

/// The model reasons as hard as its effort says. Without a summary its
/// reasoning streams as empty text.
fn thinking(drop_reasoning: bool) -> Value {
    let mut thinking = json!({"type": "adaptive", "display": "summarized"});
    if drop_reasoning {
        thinking["block_binding"] = json!({"prefix_mismatch_behavior": "drop_block"});
    }
    thinking
}

/// Where the prompt's cache is written: up to the block that carries it.
static BREAKPOINT: LazyLock<Value> = LazyLock::new(|| json!({"type": "ephemeral"}));

/// The conversation as sent. Only the newest assistant message that holds
/// thinking sends it: Claude takes the newest thinking without the older,
/// and a tool loop's older thinking is a large share of its history. A
/// message whose thinking is left out changes the prefix of every request
/// after it, so the message before the newest thinking carries a breakpoint:
/// the next request, which leaves that thinking out too, reads the cache up
/// to there.
fn conversation(messages: &[Message]) -> Vec<Said<'_>> {
    let thought = messages.iter().rposition(|message| {
        message.role == Role::Assistant && message.content.iter().any(is_thinking)
    });
    let mut conversation: Vec<Said<'_>> = Vec::new();
    for (at, message) in messages.iter().enumerate() {
        let thinks = Some(at) == thought;
        if thinks && let Some(before) = conversation.last_mut() {
            mark(&mut before.content);
        }
        conversation.extend(said(message, thinks));
    }
    conversation
}

fn is_thinking(content: &Content) -> bool {
    let Content::Other(block) = content else {
        return false;
    };
    matches!(
        block.get("type").and_then(Value::as_str),
        Some("thinking" | "redacted_thinking")
    )
}

/// Puts a breakpoint on a message's last block, if it is one that takes it.
fn mark(content: &mut [Part<'_>]) {
    if let Some(Part::Text { cache_control, .. } | Part::ToolResult { cache_control, .. }) =
        content.last_mut()
    {
        *cache_control = Some(&BREAKPOINT);
    }
}

fn said(message: &Message, thinks: bool) -> Option<Said<'_>> {
    let content: Vec<Part<'_>> = message
        .content
        .iter()
        .filter(|content| thinks || !is_thinking(content))
        .filter_map(part)
        .collect();
    let role = match message.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    (!content.is_empty()).then_some(Said { role, content })
}

/// The input of a tool use whose own is no object.
static NO_INPUT: LazyLock<Value> = LazyLock::new(|| Value::Object(Map::new()));

fn part(content: &Content) -> Option<Part<'_>> {
    Some(match content {
        // The API refuses a text block with nothing in it.
        Content::Text(text) if text.is_empty() => return None,
        Content::Text(text) => Part::Text {
            text,
            cache_control: None,
        },
        // The API takes only an object, where another API's model may have
        // written any JSON, or text that is not JSON.
        Content::ToolUse { id, name, input } => Part::ToolUse {
            id,
            name,
            input: if input.is_object() { input } else { &NO_INPUT },
        },
        // What another API wrote means nothing to this one.
        Content::Other(block)
            if crate::chat::is_reasoning(block) || crate::responses::is_item(block) =>
        {
            return None;
        }
        Content::Other(block) => Part::Other(block),
        Content::ToolResult {
            tool_use,
            text,
            error,
        } => Part::ToolResult {
            tool_use_id: tool_use,
            content: if text.is_empty() { "(no output)" } else { text },
            is_error: *error,
            cache_control: None,
        },
    })
}

#[derive(Deserialize)]
struct Reply {
    #[serde(default)]
    model: Option<String>,
    content: Vec<Value>,
    stop_reason: Option<String>,
    #[serde(default)]
    usage: ReplyUsage,
}

/// The API may write a count as null.
#[derive(Default, Deserialize)]
struct ReplyUsage {
    #[serde(default, rename = "input_tokens")]
    input: Option<u64>,
    #[serde(default, rename = "cache_creation_input_tokens")]
    cache_written: Option<u64>,
    #[serde(default, rename = "cache_read_input_tokens")]
    cache_read: Option<u64>,
    #[serde(default, rename = "output_tokens")]
    output: Option<u64>,
}

/// Reads one response.
pub(crate) fn reply(body: &str) -> Result<ModelReply, String> {
    let reply: Reply = serde_json::from_str(body).map_err(|error| error.to_string())?;
    let stop = match reply.stop_reason.as_deref() {
        Some("end_turn") => Stop::EndTurn,
        Some("tool_use") => Stop::ToolUse,
        Some("max_tokens") => Stop::MaxTokens,
        Some("refusal") => Stop::Refusal,
        _ => Stop::Other,
    };
    let usage = Usage {
        input: reply.usage.input.unwrap_or(0),
        cache_read: reply.usage.cache_read.unwrap_or(0),
        cache_written: reply.usage.cache_written.unwrap_or(0),
        output: reply.usage.output.unwrap_or(0),
        ..Usage::default()
    };
    Ok(ModelReply {
        model: reply.model,
        content: reply.content.into_iter().map(block).collect(),
        stop,
        usage,
    })
}

/// A block the mind reads.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Read {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
}

/// A text block and a tool use are read. Anything else, such as the model's
/// signed reasoning, is kept whole to be handed back.
fn block(value: Value) -> Block {
    match Read::deserialize(&value) {
        Ok(Read::Text { text }) => Block::Text { text },
        Ok(Read::ToolUse { id, name, input }) => Block::ToolUse { id, name, input },
        Err(_) => Block::Other { value },
    }
}

/// A streamed reply so far, put together as the message a request that is
/// not streamed is answered with.
#[derive(Debug, Default)]
pub(crate) struct Parts {
    message: Map<String, Value>,
    content: Vec<Value>,
    /// The input of each block that uses a tool, which comes as pieces of
    /// JSON text.
    inputs: Vec<String>,
    ended: bool,
    error: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Event {
    MessageStart {
        message: Map<String, Value>,
    },
    ContentBlockStart {
        content_block: Value,
    },
    ContentBlockDelta {
        index: usize,
        delta: Delta,
    },
    ContentBlockStop {
        index: usize,
    },
    MessageDelta {
        delta: Map<String, Value>,
        #[serde(default)]
        usage: Map<String, Value>,
    },
    MessageStop,
    Error {
        error: Value,
    },
    /// A ping, and whatever the API adds later.
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum Delta {
    #[serde(rename = "text_delta")]
    Text { text: String },
    #[serde(rename = "input_json_delta")]
    InputJson { partial_json: String },
    #[serde(rename = "thinking_delta")]
    Thinking { thinking: String },
    #[serde(rename = "signature_delta")]
    Signature { signature: String },
    #[serde(other)]
    Other,
}

impl Parts {
    /// Takes one event. Text the model wrote in it joins `text`.
    pub(crate) fn take(&mut self, data: &str, text: &mut String) {
        let event = match serde_json::from_str(data) {
            Ok(event) => event,
            Err(error) => {
                self.error = Some(format!("an event of the stream could not be read: {error}"));
                return;
            }
        };
        match event {
            Event::MessageStart { message } => self.message = message,
            Event::ContentBlockStart { content_block } => {
                // Two texts of one reply are told apart as two paragraphs.
                let is_text = content_block.get("type").and_then(Value::as_str) == Some("text");
                let follows = self.content.iter().any(|block| block.get("text").is_some());
                if is_text && follows {
                    text.push_str("\n\n");
                }
                self.content.push(content_block);
                self.inputs.push(String::new());
            }
            Event::ContentBlockDelta { index, delta } => {
                let (Some(block), Some(input)) =
                    (self.content.get_mut(index), self.inputs.get_mut(index))
                else {
                    return;
                };
                match delta {
                    Delta::Text { text: piece } => {
                        text.push_str(&piece);
                        append(block, "text", &piece);
                    }
                    Delta::InputJson { partial_json } => input.push_str(&partial_json),
                    Delta::Thinking { thinking } => append(block, "thinking", &thinking),
                    Delta::Signature { signature } => append(block, "signature", &signature),
                    Delta::Other => {}
                }
            }
            Event::ContentBlockStop { index } => {
                // An input cut off mid-way stays the empty one its block
                // started with: the reply did not stop for tools, so the
                // mind runs none of them.
                let input = self.inputs.get(index).filter(|input| !input.is_empty());
                let input = input.and_then(|input| serde_json::from_str::<Value>(input).ok());
                if let (Some(input), Some(Value::Object(block))) =
                    (input, self.content.get_mut(index))
                {
                    block.insert("input".into(), input);
                }
            }
            Event::MessageDelta { delta, usage } => {
                self.message.extend(delta);
                // Each count is the reply's so far: a later one replaces an
                // earlier, unless it is null.
                let usage = usage.into_iter().filter(|(_, count)| !count.is_null());
                match self.message.get_mut("usage") {
                    Some(Value::Object(counted)) => counted.extend(usage),
                    _ => {
                        self.message
                            .insert("usage".into(), Value::Object(usage.collect()));
                    }
                }
            }
            Event::MessageStop => self.ended = true,
            Event::Error { error } => {
                self.error = Some(crate::said(&json!({"error": error}).to_string()));
            }
            Event::Other => {}
        }
    }

    /// Whether the model has begun to answer: a block of its reply has
    /// begun.
    pub(crate) fn began(&self) -> bool {
        !self.content.is_empty()
    }

    pub(crate) fn finish(mut self) -> Result<String, String> {
        if let Some(error) = self.error {
            return Err(error);
        }
        if !self.ended {
            return Err(CUT_OFF.into());
        }
        self.message
            .insert("content".into(), Value::Array(self.content));
        Ok(Value::Object(self.message).to_string())
    }
}

/// Adds `piece` to the end of a block's string `key`.
fn append(block: &mut Value, key: &str, piece: &str) {
    match block.get_mut(key) {
        Some(Value::String(held)) => held.push_str(piece),
        _ => {
            if let Value::Object(block) = block {
                block.insert(key.into(), Value::String(piece.into()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{body_of, request};
    use crate::{Credential, Exchange};

    fn config() -> Config {
        Config::messages(Credential::ApiKey("sk-test".into()), "claude-opus-5-5")
    }

    fn written(config: &Config, request: &ModelRequest, drop_reasoning: bool) -> Value {
        serde_json::to_value(body(config, request, drop_reasoning)).unwrap()
    }

    #[test]
    fn a_request_is_written_as_the_api_reads_it() {
        assert_eq!(
            written(&config(), &request(), false),
            json!({
                "model": "claude-opus-5-5",
                "max_tokens": 16000,
                "stream": true,
                "system": "Be brief.",
                "tools": [{"name": "bash", "description": "Run a command.", "input_schema": {"type": "object"}}],
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "list the files",
                                                  "cache_control": {"type": "ephemeral"}}]},
                    {"role": "assistant", "content": [
                        {"type": "thinking", "thinking": "", "signature": "s"},
                        {"type": "tool_use", "id": "tu_1", "name": "bash", "input": {"command": "ls"}},
                        {"type": "tool_use", "id": "tu_2", "name": "bash", "input": {"command": "cat x"}},
                    ]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "tu_1", "content": "(no output)"},
                        {"type": "tool_result", "tool_use_id": "tu_2", "content": "no", "is_error": true},
                    ]},
                ],
                "cache_control": {"type": "ephemeral"},
                "fallbacks": "default",
                "thinking": {"type": "adaptive", "display": "summarized"},
            })
        );
    }

    #[test]
    fn only_the_newest_thinking_goes_back_and_the_cache_is_read_up_to_it() {
        let step = |n: u32, thinking: Vec<Value>| {
            let mut content: Vec<Content> = thinking.into_iter().map(Content::Other).collect();
            content.push(Content::ToolUse {
                id: format!("tu_{n}"),
                name: "bash".into(),
                input: json!({"command": "ls"}),
            });
            [
                Message {
                    role: Role::Assistant,
                    content,
                },
                Message {
                    role: Role::User,
                    content: vec![Content::ToolResult {
                        tool_use: format!("tu_{n}"),
                        text: format!("out {n}"),
                        error: false,
                    }],
                },
            ]
        };
        let thinking =
            |signature: &str| json!({"type": "thinking", "thinking": "", "signature": signature});
        let fallback = json!({"type": "fallback", "model": "claude-sonnet-5-5"});
        let mut request = request();
        request.messages.truncate(1);
        request.messages.extend(step(
            1,
            vec![
                thinking("s1"),
                json!({"type": "redacted_thinking", "data": "r1"}),
                fallback.clone(),
            ],
        ));
        request.messages.extend(step(2, vec![thinking("s2")]));
        // The newest step did not think: the one before it keeps its thinking.
        request.messages.extend(step(3, Vec::new()));

        let messages = written(&config(), &request, false)["messages"].clone();
        assert_eq!(messages[1]["content"][0], fallback);
        assert_eq!(messages[3]["content"][0], thinking("s2"));
        let sent: Vec<&Value> = messages
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|message| message["content"].as_array().unwrap())
            .collect();
        let kinds = |kind: &str| sent.iter().filter(|block| block["type"] == kind).count();
        assert_eq!((kinds("thinking"), kinds("redacted_thinking")), (1, 0));
        // Only the result before the kept thinking marks where the next
        // request, which leaves that thinking out, reads the cache to.
        let marked: Vec<&Value> = sent
            .iter()
            .filter(|block| block.get("cache_control").is_some())
            .map(|block| &block["content"])
            .collect();
        assert_eq!(marked, [&json!("out 1")]);
    }

    #[test]
    fn a_mind_with_a_model_of_its_own_asks_it() {
        let mut config = config();
        config.effort = Some("high".into());
        config.max_tokens = 24_000;
        let request = ModelRequest {
            model: Some("claude-haiku-5-5".into()),
            effort: Some("low".into()),
            ..request()
        };
        let body = written(&config, &request, false);
        assert_eq!(body["model"], json!("claude-haiku-5-5"));
        assert_eq!(body["output_config"], json!({"effort": "low"}));
        assert_eq!(body["max_tokens"], json!(24_000));
        // A mind that says nothing of either asks as the host says.
        let body = written(&config, &crate::tests::request(), false);
        assert_eq!(body["model"], json!("claude-opus-5-5"));
        assert_eq!(body["output_config"], json!({"effort": "high"}));
    }

    #[test]
    fn options_appear_only_when_they_are_set() {
        let mut config = config();
        config.fallbacks = false;
        config.effort = Some("high".into());
        let request = ModelRequest {
            system: None,
            tools: Vec::new(),
            text_only: true,
            ..request()
        };
        let body = written(&config, &request, true);
        assert_eq!(body["output_config"], json!({"effort": "high"}));
        assert_eq!(
            body["thinking"],
            json!({"type": "adaptive", "display": "summarized",
                   "block_binding": {"prefix_mismatch_behavior": "drop_block"}})
        );
        for absent in ["system", "tools", "fallbacks", "temperature", "tool_choice"] {
            assert!(body.get(absent).is_none(), "{absent}");
        }
    }

    #[test]
    fn a_reply_keeps_what_the_mind_does_not_read() {
        let reasoning = json!({"type": "thinking", "thinking": "", "signature": "sig"});
        let body = json!({
            "model": "claude-sonnet-5-5",
            "content": [
                reasoning,
                {"type": "text", "text": "Looking."},
                {"type": "tool_use", "id": "tu_1", "name": "bash", "input": {"command": "ls"}},
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "cache_read_input_tokens": 90,
                      "cache_creation_input_tokens": 5, "output_tokens": 7},
        });
        let reply = reply(&body.to_string()).unwrap();
        assert_eq!(
            reply.content,
            vec![
                Block::Other { value: reasoning },
                Block::Text {
                    text: "Looking.".into()
                },
                Block::ToolUse {
                    id: "tu_1".into(),
                    name: "bash".into(),
                    input: json!({"command": "ls"})
                },
            ]
        );
        assert_eq!(reply.stop, Stop::ToolUse);
        assert_eq!(reply.model.as_deref(), Some("claude-sonnet-5-5"));
        assert_eq!(
            reply.usage,
            Usage {
                input: 10,
                cache_read: 90,
                cache_written: 5,
                output: 7,
                ..Usage::default()
            }
        );
    }

    #[test]
    fn every_stop_reason_has_a_meaning() {
        let stop = |reason: Value| {
            reply(&json!({"content": [], "stop_reason": reason}).to_string())
                .unwrap()
                .stop
        };
        assert_eq!(stop(json!("end_turn")), Stop::EndTurn);
        assert_eq!(stop(json!("max_tokens")), Stop::MaxTokens);
        assert_eq!(stop(json!("refusal")), Stop::Refusal);
        assert_eq!(stop(json!("pause_turn")), Stop::Other);
        assert_eq!(stop(Value::Null), Stop::Other);
    }

    #[test]
    fn a_token_goes_on_authorization_as_a_bearer_and_nothing_more() {
        let config = Config {
            fallbacks: false,
            ..Config::messages(Credential::Token("tok".into()), "claude-opus-5-5")
        };
        let request = request();
        let post = Exchange::new(&config, &request).post();
        assert_eq!(post.header("authorization"), Some("Bearer tok"));
        assert_eq!(post.header("anthropic-beta"), None);
        assert_eq!(post.header("x-api-key"), None);
        assert_eq!(post.header("anthropic-version"), Some(VERSION));
        assert_eq!(body_of(&post)["model"], json!("claude-opus-5-5"));
    }

    #[test]
    fn what_another_api_wrote_goes_as_this_one_takes_it() {
        let mut request = request();
        request.messages[1].content[0] =
            Content::Other(json!({"type": "reasoning_details", "details": [{"text": "hm"}]}));
        // A call another API's model made with arguments that are not an
        // object goes with none.
        request.messages[1].content.push(Content::ToolUse {
            id: "call_3".into(),
            name: "bash".into(),
            input: json!("not json"),
        });
        let body = written(&config(), &request, false);
        assert_eq!(
            body["messages"][1]["content"],
            json!([
                {"type": "tool_use", "id": "tu_1", "name": "bash", "input": {"command": "ls"}},
                {"type": "tool_use", "id": "tu_2", "name": "bash", "input": {"command": "cat x"}},
                {"type": "tool_use", "id": "call_3", "name": "bash", "input": {}},
            ])
        );
    }
}

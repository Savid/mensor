//! The Responses API: `POST /responses`, as `OpenAI` speaks it.
//!
//! Nothing is stored on the server: each request carries the whole
//! conversation, and the model's reasoning comes back encrypted, to be sent
//! back with the step it came with.

use std::borrow::Cow;
use std::collections::BTreeMap;

use mensor_mind::{Block, Content, Message, ModelReply, ModelRequest, Role, Stop, Usage};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::chat::{arguments, told};
use crate::{CUT_OFF, Config, Credential, Failure, Post};

/// The request to send. With `drop_items` the step's items are left out,
/// and each step is written from its text and tool uses.
pub(crate) fn post(config: &Config, request: &ModelRequest, drop_items: bool) -> Post {
    let url = format!("{}/responses", config.base_url.trim_end_matches('/'));
    let (Credential::ApiKey(key) | Credential::Token(key)) = &config.credential;
    let body = serde_json::to_string(&body(config, request, drop_items))
        .expect("a request is strings and JSON values, which are always written");
    Post {
        url,
        headers: vec![("authorization", format!("Bearer {key}"))],
        body,
    }
}

/// Whether the API refused an item sent back to it: reasoning another
/// organization encrypted, or an item it cannot place.
pub(crate) fn items_are_refused(failure: &Failure) -> bool {
    let said = failure.detail.to_lowercase();
    failure
        .status
        .is_none_or(|status| matches!(status, 400 | 404))
        && ["encrypted content", "item '", "item with id"]
            .iter()
            .any(|refused| said.contains(refused))
}

/// Whether a block is an output item this API wrote: its reasoning, a
/// message or a tool call, kept whole to be sent back as it came.
pub(crate) fn is_item(block: &Value) -> bool {
    matches!(kind(block), Some("reasoning" | "message" | "function_call"))
}

fn kind(item: &Value) -> Option<&str> {
    item.get("type").and_then(Value::as_str)
}

/// The body of one request. It borrows the conversation.
#[derive(Serialize)]
struct Body<'a> {
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<&'a str>,
    input: Vec<Item<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ToolDef<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<Reasoning<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    stream: bool,
    store: bool,
    /// The model's reasoning, encrypted, which a server that stores
    /// nothing may otherwise leave out.
    include: [&'static str; 1],
}

#[derive(Serialize)]
struct ToolDef<'a> {
    r#type: &'static str,
    name: &'a str,
    description: &'a str,
    parameters: &'a Value,
    /// The API takes a tool as strict unless told, and a strict tool must
    /// name every property required.
    strict: bool,
}

#[derive(Serialize)]
struct Reasoning<'a> {
    effort: &'a str,
    summary: &'static str,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Item<'a> {
    Message {
        role: &'static str,
        content: Vec<Part<'a>>,
    },
    FunctionCall {
        call_id: &'a str,
        name: &'a str,
        /// The input, as JSON in a string.
        arguments: String,
    },
    FunctionCallOutput {
        call_id: &'a str,
        output: Cow<'a, str>,
    },
    /// An output item of the model's own, written as it came.
    #[serde(untagged)]
    Kept(&'a Value),
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Part<'a> {
    InputText { text: &'a str },
    OutputText { text: &'a str },
}

fn body<'a>(config: &'a Config, request: &'a ModelRequest, drop_items: bool) -> Body<'a> {
    let mut input = Vec::new();
    for message in &request.messages {
        match message.role {
            Role::User => user(&mut input, message),
            Role::Assistant => assistant(&mut input, message, drop_items),
        }
    }
    Body {
        model: request.model.as_deref().unwrap_or(&config.model),
        instructions: request
            .system
            .as_deref()
            .filter(|system| !system.is_empty()),
        input,
        tools: request
            .tools
            .iter()
            .map(|tool| ToolDef {
                r#type: "function",
                name: &tool.name,
                description: &tool.description,
                parameters: &tool.input,
                strict: false,
            })
            .collect(),
        tool_choice: (request.text_only && !request.tools.is_empty()).then_some("none"),
        // No effort leaves the model its own, and a model that does not
        // reason is sent nothing it would refuse.
        reasoning: request
            .effort
            .as_deref()
            .or(config.effort.as_deref())
            .map(|effort| Reasoning {
                effort,
                summary: "auto",
            }),
        max_output_tokens: config.max_tokens,
        stream: true,
        store: false,
        include: ["reasoning.encrypted_content"],
    }
}

/// Each result answers the call it names; the text follows as one message.
fn user<'a>(input: &mut Vec<Item<'a>>, message: &'a Message) {
    let mut said = Vec::new();
    for content in &message.content {
        match content {
            Content::ToolResult {
                tool_use,
                text,
                error,
            } => input.push(Item::FunctionCallOutput {
                call_id: tool_use,
                output: told(text, *error),
            }),
            Content::Text(text) if !text.is_empty() => said.push(Part::InputText { text }),
            Content::Text(_) | Content::ToolUse { .. } | Content::Other(_) => {}
        }
    }
    if !said.is_empty() {
        input.push(Item::Message {
            role: "user",
            content: said,
        });
    }
}

/// A step's output items go back as they came, each in place of the text or
/// tool use read from it, right after it, which carries less: a message's
/// phase, and a call's arguments as the model wrote them. A step asked under
/// another model or setup has none, and is written from its text and tool
/// uses.
fn assistant<'a>(input: &mut Vec<Item<'a>>, message: &'a Message, drop_items: bool) {
    let mut before: Option<&Value> = None;
    for content in &message.content {
        let kept = before.take();
        match content {
            Content::Other(item) if !drop_items && is_item(item) => {
                input.push(Item::Kept(item));
                before = Some(item);
            }
            Content::Text(_) if kept.and_then(kind) == Some("message") => {}
            Content::Text(text) if !text.is_empty() => input.push(Item::Message {
                role: "assistant",
                content: vec![Part::OutputText { text }],
            }),
            Content::ToolUse { id, .. }
                if kept
                    .and_then(|item| item.get("call_id"))
                    .and_then(Value::as_str)
                    == Some(id) => {}
            Content::ToolUse {
                id,
                name,
                input: asked,
            } => input.push(Item::FunctionCall {
                call_id: id,
                name,
                arguments: arguments(asked),
            }),
            // What another API wrote means nothing to this one.
            Content::Text(_) | Content::Other(_) | Content::ToolResult { .. } => {}
        }
    }
}

#[derive(Deserialize)]
struct Reply {
    /// The model that answered.
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    error: Option<Value>,
    #[serde(default)]
    incomplete_details: Option<Incomplete>,
    #[serde(default)]
    output: Vec<Value>,
    #[serde(default)]
    usage: Option<ReplyUsage>,
}

#[derive(Deserialize)]
struct Incomplete {
    reason: Option<String>,
}

#[derive(Deserialize)]
struct ReplyUsage {
    /// Every token read, those of the cache included.
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    input_tokens_details: Option<InputDetails>,
    #[serde(default)]
    output_tokens: Option<u64>,
}

#[derive(Default, Deserialize)]
struct InputDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
    /// Written by a server that charges for writing the cache.
    #[serde(default)]
    cache_write_tokens: Option<u64>,
}

/// An output item the mind reads.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Read {
    Reasoning,
    Message {
        #[serde(default)]
        content: Vec<Said>,
    },
    FunctionCall {
        call_id: String,
        name: String,
        #[serde(default)]
        arguments: String,
    },
    /// Anything else, such as a server's own tool, the mind neither reads
    /// nor sends back.
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Said {
    OutputText {
        text: String,
    },
    Refusal {
        refusal: String,
    },
    #[serde(other)]
    Other,
}

/// Reads one response.
pub(crate) fn reply(body: &str) -> Result<ModelReply, String> {
    let reply: Reply = serde_json::from_str(body).map_err(|error| error.to_string())?;
    let ended = match reply.status.as_deref() {
        Some("failed") => Some("failed"),
        Some("cancelled") => Some("was cancelled"),
        _ => None,
    };
    if let Some(ended) = ended {
        return Err(format!(
            "the response {ended}: {}",
            failed(reply.error.as_ref())
        ));
    }
    // A reply cut off has no item to follow its reasoning, and the API
    // refuses reasoning sent back without it: its items are not kept.
    let whole = reply.status.as_deref() != Some("incomplete");
    let mut content = Vec::new();
    let (mut called, mut refused) = (false, false);
    for item in reply.output {
        let read = Read::deserialize(&item).map_err(|error| error.to_string())?;
        let block = match read {
            Read::Reasoning => {
                if whole {
                    content.push(Block::Other { value: item });
                }
                continue;
            }
            Read::Message { content: said } => {
                let mut text = String::new();
                for part in said {
                    match part {
                        Said::OutputText { text: piece } => text.push_str(&piece),
                        Said::Refusal { refusal } => {
                            refused = true;
                            text.push_str(&refusal);
                        }
                        Said::Other => {}
                    }
                }
                (!text.is_empty()).then_some(Block::Text { text })
            }
            Read::FunctionCall {
                call_id,
                name,
                arguments,
            } => {
                called = true;
                Some(Block::ToolUse {
                    id: call_id,
                    name,
                    input: crate::chat::input(arguments),
                })
            }
            Read::Other => None,
        };
        let Some(block) = block else {
            unfollowed(&mut content);
            continue;
        };
        if whole {
            content.push(Block::Other { value: item });
        }
        content.push(block);
    }
    unfollowed(&mut content);

    let reason = reply.incomplete_details.and_then(|details| details.reason);
    let stop = match reason.as_deref() {
        Some("max_output_tokens") => Stop::MaxTokens,
        Some("content_filter") => Stop::Refusal,
        // The calls of a reply cut off are not run.
        _ if !whole => Stop::Other,
        _ if called => Stop::ToolUse,
        _ if refused => Stop::Refusal,
        _ => Stop::EndTurn,
    };
    let usage = reply.usage.map_or_else(Usage::default, |usage| {
        let read_in = usage.input_tokens.unwrap_or(0);
        let cache = usage.input_tokens_details.unwrap_or_default();
        let read = cache.cached_tokens.unwrap_or(0);
        let written = cache.cache_write_tokens.unwrap_or(0);
        Usage {
            // A server whose input count leaves the cache out has it all.
            input: read_in.checked_sub(read + written).unwrap_or(read_in),
            cache_read: read,
            cache_written: written,
            output: usage.output_tokens.unwrap_or(0),
        }
    });
    Ok(ModelReply {
        model: reply.model,
        content,
        stop,
        usage,
    })
}

/// Leaves out reasoning kept last: it goes back only with the item that
/// followed it, and that one is not kept.
fn unfollowed(content: &mut Vec<Block>) {
    while let Some(Block::Other { value }) = content.last() {
        if kind(value) != Some("reasoning") {
            break;
        }
        content.pop();
    }
}

/// A streamed reply so far, put together as the response a request that is
/// not streamed is answered with.
#[derive(Debug, Default)]
pub(crate) struct Parts {
    /// The response its last event ends with.
    response: Option<Map<String, Value>>,
    /// Each output item as it ended, by its index: the ones to send back.
    done: BTreeMap<u64, Value>,
    /// The index of the message whose text came last.
    writing: Option<u64>,
    error: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum Event {
    #[serde(rename = "response.output_text.delta")]
    Text { output_index: u64, delta: String },
    #[serde(rename = "response.output_item.done")]
    ItemDone { output_index: u64, item: Value },
    #[serde(rename = "response.completed")]
    Completed { response: Map<String, Value> },
    #[serde(rename = "response.incomplete")]
    Incomplete { response: Map<String, Value> },
    #[serde(rename = "response.failed")]
    Failed { response: Map<String, Value> },
    #[serde(rename = "error")]
    Error(Map<String, Value>),
    /// Every other event, and whatever the API adds later.
    #[serde(other)]
    Other,
}

impl Parts {
    /// Takes one event. Text the model wrote in it joins `text`.
    pub(crate) fn take(&mut self, data: &str, text: &mut String) {
        if data == "[DONE]" {
            return;
        }
        let event = match serde_json::from_str(data) {
            Ok(event) => event,
            Err(error) => {
                self.error = Some(format!("an event of the stream could not be read: {error}"));
                return;
            }
        };
        match event {
            Event::Text {
                output_index,
                delta,
            } => {
                // Two messages of one reply are told apart as two
                // paragraphs.
                if self.writing.is_some_and(|index| index != output_index) {
                    text.push_str("\n\n");
                }
                self.writing = Some(output_index);
                text.push_str(&delta);
            }
            Event::ItemDone { output_index, item } => {
                self.done.insert(output_index, item);
            }
            Event::Completed { response } => self.response = Some(response),
            Event::Incomplete { mut response } => {
                if !response.get("status").is_some_and(Value::is_string) {
                    response.insert("status".into(), json!("incomplete"));
                }
                self.response = Some(response);
            }
            Event::Failed { response } => self.error = Some(failed(response.get("error"))),
            // `{type, code, message}`, or the same nested in `error`.
            Event::Error(mut error) => {
                error.remove("type");
                let error = error.remove("error").unwrap_or(Value::Object(error));
                self.error = Some(failed(Some(&error)));
            }
            Event::Other => {}
        }
    }

    pub(crate) fn finish(self) -> Result<String, String> {
        if let Some(error) = self.error {
            return Err(error);
        }
        let Some(mut response) = self.response else {
            return Err(CUT_OFF.into());
        };
        // An item missing among those that ended was lost on the way.
        if !self.done.is_empty() {
            let listed = response
                .get("output")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            let unbroken = self.done.keys().zip(0..).all(|(index, at)| *index == at);
            if !unbroken || self.done.len() < listed {
                return Err(CUT_OFF.into());
            }
            let output = self.done.into_values().collect();
            response.insert("output".into(), Value::Array(output));
        }
        Ok(Value::Object(response).to_string())
    }
}

/// What a failed response or an error event says: its type or code, and
/// its message.
fn failed(error: Option<&Value>) -> String {
    let said = |key: &str| {
        error
            .and_then(|error| error.get(key))
            .and_then(Value::as_str)
    };
    match (said("type").or_else(|| said("code")), said("message")) {
        (Some(kind), Some(message)) => format!("{kind}: {message}"),
        (None, Some(message)) => message.to_owned(),
        (Some(kind), None) => kind.to_owned(),
        (None, None) => "the response failed and said nothing of why".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Exchange;
    use crate::tests::{body_of, reasoning, request};

    fn said(id: &str, text: &str) -> Value {
        json!({"type": "message", "id": id, "role": "assistant", "status": "completed",
               "phase": "commentary",
               "content": [{"type": "output_text", "annotations": [], "text": text}]})
    }

    fn call(id: &str, arguments: &str) -> Value {
        json!({"type": "function_call", "id": format!("fc_{id}"), "status": "completed",
               "call_id": id, "name": "bash", "arguments": arguments})
    }

    fn output(call: &str, output: &str) -> Value {
        json!({"type": "function_call_output", "call_id": call, "output": output})
    }

    fn user_said(text: &str) -> Value {
        json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]})
    }

    /// A step as the window hands it back: what a reply of these output
    /// items was read as.
    fn step(output: &[Value]) -> Message {
        let replied = json!({"status": "completed", "output": output});
        let content = reply(&replied.to_string()).unwrap().content;
        let content = content
            .into_iter()
            .map(|block| match block {
                Block::Text { text } => Content::Text(text),
                Block::ToolUse { id, name, input } => Content::ToolUse { id, name, input },
                Block::Other { value } => Content::Other(value),
            })
            .collect();
        Message {
            role: Role::Assistant,
            content,
        }
    }

    fn results(said: &[(&str, &str)], then: &str) -> Message {
        let mut content: Vec<Content> = said
            .iter()
            .map(|(call, text)| Content::ToolResult {
                tool_use: (*call).into(),
                text: (*text).into(),
                error: false,
            })
            .collect();
        content.push(Content::Text(then.into()));
        Message {
            role: Role::User,
            content,
        }
    }

    #[test]
    fn a_reply_goes_back_as_it_came() {
        let replied = vec![
            reasoning(),
            said("msg_1", "Looking."),
            call("call_1", "{\"command\": \"ls\"}"),
            call("call_2", "{}"),
        ];
        let request = ModelRequest {
            messages: vec![
                results(&[], "list the files"),
                step(&replied),
                results(&[("call_1", "a"), ("call_2", "b")], "go on"),
            ],
            ..request()
        };
        let config = Config::responses("sk-test", "gpt");
        let body = body_of(&Exchange::new(&config, &request).post());
        // Each item once, in its place, and each call answered.
        let mut sent = vec![user_said("list the files")];
        sent.extend(replied);
        sent.extend([
            output("call_1", "a"),
            output("call_2", "b"),
            user_said("go on"),
        ]);
        assert_eq!(body["input"], Value::Array(sent));
    }

    #[test]
    fn a_request_is_written_as_the_api_reads_it() {
        let mut config = Config::responses("sk-test", "gpt");
        config.effort = Some("high".into());
        let mut kept = step(&[
            said("msg_1", "Listing."),
            call("call_9", "{\"command\": \"ls -a\"}"),
        ]);
        // A text or a tool use with no item of its own goes as itself.
        kept.content.insert(2, Content::Text("And more.".into()));
        kept.content.push(Content::ToolUse {
            id: "call_10".into(),
            name: "bash".into(),
            input: json!("not json"),
        });
        let mut request = request();
        request
            .messages
            .extend([kept, results(&[("call_9", "a"), ("call_10", "b")], "go on")]);
        let post = Exchange::new(&config, &request).post();
        assert_eq!(post.url, "https://api.openai.com/v1/responses");
        assert_eq!(post.header("authorization"), Some("Bearer sk-test"));
        let written = |call: &str, arguments: &str| {
            json!({"type": "function_call", "call_id": call, "name": "bash",
                   "arguments": arguments})
        };
        assert_eq!(
            body_of(&post),
            json!({
                "model": "gpt",
                "instructions": "Be brief.",
                "input": [
                    user_said("list the files"),
                    // Its own items go back as they came, and Claude's
                    // thinking not at all.
                    reasoning(),
                    written("tu_1", "{\"command\":\"ls\"}"),
                    written("tu_2", "{\"command\":\"cat x\"}"),
                    output("tu_1", "(no output)"),
                    output("tu_2", "Error: no"),
                    said("msg_1", "Listing."),
                    {"type": "message", "role": "assistant",
                     "content": [{"type": "output_text", "text": "And more."}]},
                    call("call_9", "{\"command\": \"ls -a\"}"),
                    written("call_10", "not json"),
                    output("call_9", "a"),
                    output("call_10", "b"),
                    user_said("go on"),
                ],
                "tools": [{"type": "function", "name": "bash", "description": "Run a command.",
                           "parameters": {"type": "object"}, "strict": false}],
                "reasoning": {"effort": "high", "summary": "auto"},
                "max_output_tokens": 16000,
                "stream": true,
                "store": false,
                "include": ["reasoning.encrypted_content"],
            })
        );

        // Text alone, at the model's own effort, with no cap.
        config.effort = None;
        config.max_tokens = None;
        request.text_only = true;
        let body = body_of(&Exchange::new(&config, &request).post());
        assert_eq!(body["tool_choice"], json!("none"));
        assert!(body.get("reasoning").is_none());
        assert!(body.get("max_output_tokens").is_none());
    }

    #[test]
    fn a_reply_keeps_only_what_can_go_back_and_stops_as_it_ended() {
        let read = |replied: Value| reply(&replied.to_string());
        let text = |text: &str| Block::Text { text: text.into() };
        let empty = json!({"type": "message", "role": "assistant", "content": []});

        // Reasoning goes back only with the item that followed it.
        let ended = json!({"status": "completed",
                           "output": [reasoning(), empty, said("msg_1", "Hi"), reasoning()]});
        let ended = read(ended).unwrap();
        assert_eq!(
            ended.content,
            vec![
                Block::Other {
                    value: said("msg_1", "Hi")
                },
                text("Hi")
            ]
        );
        assert_eq!(ended.stop, Stop::EndTurn);

        // A reply cut off keeps no item, and runs none of its calls.
        for (details, stop) in [
            (json!({"reason": "max_output_tokens"}), Stop::MaxTokens),
            (json!({"reason": "content_filter"}), Stop::Refusal),
            (Value::Null, Stop::Other),
        ] {
            let cut = json!({"status": "incomplete", "incomplete_details": details,
                             "output": [reasoning(), said("msg_1", "Partly"),
                                        call("call_1", "{\"comm")]});
            let cut = read(cut).unwrap();
            let asked = Block::ToolUse {
                id: "call_1".into(),
                name: "bash".into(),
                input: json!("{\"comm"),
            };
            assert_eq!(cut.content, vec![text("Partly"), asked]);
            assert_eq!(cut.stop, stop);
        }

        let refusal = json!({"type": "message", "role": "assistant",
                             "content": [{"type": "refusal", "refusal": "No."}]});
        let refused = json!({"status": "completed", "output": [refusal],
                             "usage": {"input_tokens": 100, "output_tokens": 2,
                                       "input_tokens_details": {"cached_tokens": 60,
                                                                "cache_write_tokens": 30}}});
        let refused = read(refused).unwrap();
        assert_eq!(refused.content.last(), Some(&text("No.")));
        assert_eq!(refused.stop, Stop::Refusal);
        assert_eq!(
            refused.usage,
            Usage {
                input: 10,
                cache_read: 60,
                cache_written: 30,
                output: 2
            }
        );

        for status in ["failed", "cancelled"] {
            assert!(read(json!({"status": status, "error": null, "output": []})).is_err());
        }
    }
}

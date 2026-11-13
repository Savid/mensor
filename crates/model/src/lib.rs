//! Model APIs, as requests to send and answers to read.
//!
//! A host speaks a model's HTTP API itself: one request per model call.
//! This crate holds everything about that except the sending, for two APIs:
//! Claude's Messages API, and the Chat Completions API that `OpenAI`,
//! `OpenRouter` and most other servers speak. An [`Exchange`] is one model
//! call: it says what to post, is told what came back, and answers with the
//! reply or with how long to wait before posting again.
//!
//! Every request asks for its reply as a stream of server-sent events. A
//! host hands the bytes of one to a [`Reading`] as they arrive, which says
//! what text the model has written so far, and at the end gives the body an
//! API that does not stream would have sent.
//!
//! ```
//! use mensor_mind::{Content, Message, ModelRequest, Role};
//! use mensor_model::{Answer, Config, Credential, Exchange, Next};
//!
//! let config = Config::messages(Credential::ApiKey("sk-…".into()), "claude-opus-5-5");
//! let request = ModelRequest {
//!     seen: 1,
//!     system: None,
//!     messages: vec![Message { role: Role::User, content: vec![Content::Text("hi".into())] }],
//!     tools: Vec::new(),
//!     text_only: false,
//! };
//! let mut exchange = Exchange::new(&config, &request);
//!
//! let post = exchange.post();
//! assert_eq!(post.url, "https://api.anthropic.com/v1/messages");
//! // The host sends `post`, and says what came back.
//! let answer = Answer::Response {
//!     status: 200,
//!     retry_after: None,
//!     request_id: None,
//!     body: r#"{"content":[{"type":"text","text":"hello"}],"stop_reason":"end_turn"}"#.into(),
//! };
//! assert!(matches!(exchange.answered(answer), Next::Done(Ok(_))));
//! ```
//!
//! A request is written straight from the conversation it borrows, so a long
//! history is copied once, into the bytes that are sent.
//!
//! This crate is pure: no I/O, no clock and no randomness.

mod chat;
mod messages;

use std::fmt::{self, Write as _};
use std::time::Duration;

use mensor_mind::{ModelError, ModelReply, ModelRequest};
use serde::Deserialize;

pub use messages::{BINDING_BETA, FALLBACK_BETA, OAUTH_BETA, VERSION};

/// The API a model is asked through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Api {
    /// Claude's Messages API: `POST {base}/v1/messages`.
    Messages,
    /// Chat Completions: `POST {base}/chat/completions`.
    Chat,
}

/// How to prove who is asking.
#[derive(Clone)]
pub enum Credential {
    /// An API key. The Messages API takes it as `x-api-key`, Chat
    /// Completions as a bearer token.
    ApiKey(String),
    /// An OAuth access token, sent as a bearer token.
    Token(String),
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ApiKey(_) => "ApiKey(..)",
            Self::Token(_) => "Token(..)",
        })
    }
}

/// How a model is asked.
#[derive(Clone, Debug)]
pub struct Config {
    /// The API the model is behind.
    pub api: Api,
    /// The API's address: without a path for the Messages API, and up to
    /// and including the version, such as `/v1`, for Chat Completions.
    pub base_url: String,
    /// The credential every request carries.
    pub credential: Credential,
    /// The model's id, such as `claude-opus-5-5`.
    pub model: String,
    /// The most tokens one reply may hold, which the Messages API must be
    /// told. A reply that reaches it is cut off, and the mind asks again.
    /// A Chat Completions server is left to its own limit.
    pub max_tokens: u32,
    /// How hard the model works, in the API's own words: for Claude `low`,
    /// `medium`, `high`, `xhigh` or `max`. `None` leaves the model's own
    /// default.
    pub effort: Option<String>,
    /// Whether a request Claude declines may be served by the fallback
    /// model the Messages API picks. Turn it off where the API does not
    /// offer it. Chat Completions has none.
    pub fallbacks: bool,
    /// How many times a failed request is sent again.
    pub max_retries: u32,
    /// How long a host lets one request take.
    pub timeout: Duration,
}

impl Config {
    /// The defaults for `model` behind the Messages API.
    pub fn messages(credential: Credential, model: impl Into<String>) -> Self {
        Self {
            api: Api::Messages,
            base_url: "https://api.anthropic.com".into(),
            credential,
            model: model.into(),
            max_tokens: 16_000,
            effort: None,
            fallbacks: true,
            max_retries: 4,
            timeout: Duration::from_mins(10),
        }
    }

    /// The defaults for `model` behind Chat Completions.
    pub fn chat(key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            api: Api::Chat,
            base_url: "https://api.openai.com/v1".into(),
            ..Self::messages(Credential::ApiKey(key.into()), model)
        }
    }

    /// The defaults for `model`, with the API, credential and address that
    /// `var` gives. A host passes its own source of variables: its
    /// environment, or the bindings a platform gives it.
    ///
    /// `ANTHROPIC_API_KEY` or else `ANTHROPIC_AUTH_TOKEN` is the Messages
    /// API, at `ANTHROPIC_BASE_URL` when that is set. `OPENAI_API_KEY` is
    /// Chat Completions, at `OPENAI_BASE_URL` when that is set. With both,
    /// it is the Messages API unless `MENSOR_API` is `chat`.
    pub fn from_vars(
        model: impl Into<String>,
        var: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, ModelError> {
        let set = |name: &str| var(name).filter(|value| !value.is_empty());
        let asked = match set("MENSOR_API").as_deref() {
            None => None,
            Some("messages") => Some(Api::Messages),
            Some("chat") => Some(Api::Chat),
            Some(other) => {
                return Err(ModelError(format!(
                    "MENSOR_API is messages or chat, not {other}"
                )));
            }
        };
        let claude = match (set("ANTHROPIC_API_KEY"), set("ANTHROPIC_AUTH_TOKEN")) {
            (Some(key), _) => Some(Credential::ApiKey(key)),
            (None, Some(token)) => Some(Credential::Token(token)),
            (None, None) => None,
        };
        let (mut config, address) = match (asked, claude, set("OPENAI_API_KEY")) {
            (None | Some(Api::Messages), Some(credential), _) => {
                (Self::messages(credential, model), "ANTHROPIC_BASE_URL")
            }
            (None | Some(Api::Chat), _, Some(key)) => (Self::chat(key, model), "OPENAI_BASE_URL"),
            _ => {
                return Err(ModelError(
                    "set ANTHROPIC_API_KEY or ANTHROPIC_AUTH_TOKEN to reach a model through the \
                     Messages API, or OPENAI_API_KEY to reach one through Chat Completions"
                        .into(),
                ));
            }
        };
        if let Some(base_url) = set(address) {
            config.base_url = base_url;
        }
        Ok(config)
    }
}

/// One request for a host to send.
#[derive(Clone, PartialEq, Eq)]
pub struct Post {
    /// Where it goes.
    pub url: String,
    /// Its headers, besides `content-type`. One of them holds the
    /// credential, so a host never logs them.
    pub headers: Vec<(&'static str, String)>,
    /// Its body, which is JSON.
    pub body: String,
}

impl Post {
    /// The value of the header `name`.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(known, _)| *known == name)
            .map(|(_, value)| value.as_str())
    }
}

impl fmt::Debug for Post {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.headers.iter().map(|(name, _)| *name).collect();
        f.debug_struct("Post")
            .field("url", &self.url)
            .field("headers", &names)
            .finish_non_exhaustive()
    }
}

/// What came back from a [`Post`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// The API answered.
    Response {
        /// The HTTP status.
        status: u16,
        /// The `retry-after` header, if there was one.
        retry_after: Option<String>,
        /// The `request-id` or `x-request-id` header, if there was one.
        request_id: Option<String>,
        /// The body, whole.
        body: String,
    },
    /// Nothing came back: the request did not arrive, or its answer was
    /// lost or took too long.
    Unreachable(String),
}

/// What a host does after an [`Answer`].
#[derive(Clone, Debug, PartialEq)]
pub enum Next {
    /// The call is over.
    Done(Result<ModelReply, ModelError>),
    /// Wait this long, then send [`Exchange::post`] again.
    Again {
        /// How long to wait.
        wait: Duration,
        /// Why the last request did not end the call, for a log.
        why: ModelError,
    },
}

/// One model call: a request, and the retries it may take.
#[derive(Clone, Debug)]
pub struct Exchange<'a> {
    config: &'a Config,
    request: &'a ModelRequest,
    attempt: u32,
    drop_reasoning: bool,
}

impl<'a> Exchange<'a> {
    /// A call that has sent nothing yet.
    pub fn new(config: &'a Config, request: &'a ModelRequest) -> Self {
        Self {
            config,
            request,
            attempt: 0,
            drop_reasoning: false,
        }
    }

    /// The request to send now.
    pub fn post(&self) -> Post {
        match self.config.api {
            Api::Messages => messages::post(self.config, self.request, self.drop_reasoning),
            Api::Chat => chat::post(self.config, self.request),
        }
    }

    /// Reads the answer to the last [`Exchange::post`], when it comes as
    /// `text/event-stream`.
    pub fn reading(&self) -> Reading {
        Reading {
            line: Vec::new(),
            parts: match self.config.api {
                Api::Messages => Parts::Messages(messages::Parts::default()),
                Api::Chat => Parts::Chat(chat::Parts::default()),
            },
            written: 0,
        }
    }

    /// Takes what came back from the last [`Exchange::post`].
    ///
    /// A request that never arrived, or that the API was too busy for, is
    /// worth sending again, up to [`Config::max_retries`] times. Claude
    /// refuses reasoning the history no longer matches: the mind never
    /// edits its history, but the system prompt or the body's tools can
    /// change between requests, so those blocks are dropped once and the
    /// request sent again.
    pub fn answered(&mut self, answer: Answer) -> Next {
        let failure = match answer {
            Answer::Response { status, body, .. } if (200..300).contains(&status) => {
                let read = match self.config.api {
                    Api::Messages => messages::reply(&body),
                    Api::Chat => chat::reply(&body),
                };
                return Next::Done(read.map_err(|error| {
                    ModelError(format!("the model's reply could not be read: {error}"))
                }));
            }
            Answer::Response {
                status,
                retry_after,
                request_id,
                body,
            } => Failure {
                status: Some(status),
                wait: retry_after.as_deref().and_then(asked_wait),
                detail: said(&body),
                request_id,
            },
            Answer::Unreachable(detail) => Failure {
                status: None,
                wait: None,
                detail,
                request_id: None,
            },
        };

        let unbound = self.config.api == Api::Messages && messages::reasoning_is_unbound(&failure);
        if unbound && !self.drop_reasoning {
            self.drop_reasoning = true;
            return Next::Again {
                wait: Duration::ZERO,
                why: failure.into(),
            };
        }
        if !failure.is_retryable() || self.attempt >= self.config.max_retries {
            return Next::Done(Err(failure.into()));
        }
        let wait = failure.wait.unwrap_or_else(|| backoff(self.attempt));
        self.attempt += 1;
        Next::Again {
            wait,
            why: failure.into(),
        }
    }
}

/// A reply that arrives as server-sent events, read as it comes.
#[derive(Debug)]
pub struct Reading {
    /// The bytes of a line that has not ended yet.
    line: Vec<u8>,
    parts: Parts,
    /// How many bytes of text the model has written.
    written: u64,
}

#[derive(Debug)]
enum Parts {
    Messages(messages::Parts),
    Chat(chat::Parts),
}

/// What is wrong with a stream that stops before its reply has ended.
const CUT_OFF: &str = "the reply was cut off before it ended";

impl Reading {
    /// Takes the next bytes of the answer, however they are cut. If the
    /// model wrote text in them, `wrote` is told once: how many bytes of
    /// text came before it, and the text.
    pub fn read(&mut self, bytes: &[u8], wrote: &mut dyn FnMut(u64, &str)) {
        self.line.extend_from_slice(bytes);
        let mut text = String::new();
        let mut read = 0;
        while let Some(end) = self.line[read..].iter().position(|byte| *byte == b'\n') {
            // An event's `data` line holds it whole. The lines that name
            // the event, comment or end it say nothing more.
            let line = std::str::from_utf8(&self.line[read..read + end]).ok();
            let data = line.and_then(|line| line.trim_end_matches('\r').strip_prefix("data:"));
            if let Some(data) = data.map(str::trim_start) {
                match &mut self.parts {
                    Parts::Messages(parts) => parts.take(data, &mut text),
                    Parts::Chat(parts) => parts.take(data, &mut text),
                }
            }
            read += end + 1;
        }
        self.line.drain(..read);
        if !text.is_empty() {
            wrote(self.written, &text);
            self.written += u64::try_from(text.len()).unwrap_or(u64::MAX);
        }
    }

    /// The body the stream amounts to, as an API that does not stream
    /// would have sent it, for [`Answer::Response`]. An error is a reply
    /// that never ended, which is told as [`Answer::Unreachable`].
    pub fn finish(self) -> Result<String, String> {
        match self.parts {
            Parts::Messages(parts) => parts.finish(),
            Parts::Chat(parts) => parts.finish(),
        }
    }
}

/// A request that did not end in a reply.
struct Failure {
    status: Option<u16>,
    wait: Option<Duration>,
    detail: String,
    request_id: Option<String>,
}

impl Failure {
    /// A request worth sending again: it never arrived, or the API says it
    /// is busy or failed.
    ///
    /// An account with nothing left to spend is answered as a busy API is,
    /// with 429, and no wait fills it: that one is not sent again.
    fn is_retryable(&self) -> bool {
        let spent = self.detail.starts_with("insufficient_quota");
        !spent
            && self
                .status
                .is_none_or(|status| matches!(status, 408 | 409 | 429 | 500..=599))
    }
}

impl From<Failure> for ModelError {
    fn from(failure: Failure) -> Self {
        let mut error = match failure.status {
            Some(status) => format!("the model answered {status}: {}", failure.detail),
            None => format!("the model could not be reached: {}", failure.detail),
        };
        if let Some(request_id) = failure.request_id {
            let _ = write!(error, " (request {request_id})");
        }
        Self(error)
    }
}

/// The wait the API asks for in a `retry-after` header, at most two minutes.
fn asked_wait(header: &str) -> Option<Duration> {
    let seconds: f64 = header.trim().parse().ok()?;
    (seconds.is_finite() && seconds >= 0.0).then(|| Duration::from_secs_f64(seconds.min(120.0)))
}

/// One second, doubling with each attempt up to thirty.
fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(1 << attempt.min(5)).min(Duration::from_secs(30))
}

/// The message of an API error body, or the body itself when it is not one.
/// Both APIs write `{"error": {"type", "message"}}`, the type sometimes
/// left out.
fn said(body: &str) -> String {
    #[derive(Deserialize)]
    struct Body {
        error: Error,
    }
    #[derive(Deserialize)]
    struct Error {
        r#type: Option<String>,
        message: String,
    }
    match serde_json::from_str::<Body>(body) {
        Ok(Body {
            error:
                Error {
                    r#type: Some(kind),
                    message,
                },
        }) => format!("{kind}: {message}"),
        Ok(Body { error }) => error.message,
        Err(_) => body.chars().take(500).collect(),
    }
}

#[cfg(test)]
mod tests {
    use mensor_mind::{Block, Content, Message, Role, Tool};
    use serde_json::{Value, json};

    use super::*;

    pub(crate) fn request() -> ModelRequest {
        ModelRequest {
            seen: 3,
            system: Some("Be brief.".into()),
            messages: vec![
                Message {
                    role: Role::User,
                    content: vec![Content::Text("list the files".into())],
                },
                Message {
                    role: Role::Assistant,
                    content: vec![
                        Content::Other(
                            json!({"type": "thinking", "thinking": "", "signature": "s"}),
                        ),
                        Content::ToolUse {
                            id: "tu_1".into(),
                            name: "bash".into(),
                            input: json!({"command": "ls"}),
                        },
                    ],
                },
                Message {
                    role: Role::User,
                    content: vec![
                        Content::ToolResult {
                            tool_use: "tu_1".into(),
                            text: String::new(),
                            error: false,
                        },
                        Content::ToolResult {
                            tool_use: "tu_2".into(),
                            text: "no".into(),
                            error: true,
                        },
                        Content::Text(String::new()),
                    ],
                },
            ],
            tools: vec![Tool {
                name: "bash".into(),
                description: "Run a command.".into(),
                input: json!({"type": "object"}),
                capability: "commands".into(),
                operation: "bash".into(),
            }],
            text_only: false,
        }
    }

    pub(crate) fn body_of(post: &Post) -> Value {
        serde_json::from_str(&post.body).expect("a request body is JSON")
    }

    fn config() -> Config {
        Config::messages(Credential::ApiKey("sk-test".into()), "claude-opus-5-5")
    }

    fn answer(status: u16, body: &Value) -> Answer {
        Answer::Response {
            status,
            retry_after: None,
            request_id: None,
            body: body.to_string(),
        }
    }

    fn hello() -> Value {
        json!({
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1},
        })
    }

    fn error(kind: &str, message: &str) -> Value {
        json!({"type": "error", "error": {"type": kind, "message": message}})
    }

    #[test]
    fn a_credential_is_never_printed() {
        let config = config();
        let request = request();
        let post = Exchange::new(&config, &request).post();
        assert_eq!(post.header("x-api-key"), Some("sk-test"));
        for printed in [format!("{config:?}"), format!("{post:?}")] {
            assert!(!printed.contains("sk-test"), "{printed}");
        }
    }

    /// A source of variables that holds `set`.
    fn vars<'a>(set: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            let found = set.iter().find(|(known, _)| *known == name);
            found.map(|(_, value)| (*value).to_owned())
        }
    }

    #[test]
    fn variables_choose_the_api_the_credential_and_the_address() {
        let request = request();
        let url = |config: &Config| Exchange::new(config, &request).post().url;

        let claude = Config::from_vars(
            "claude-opus-5-5",
            vars(&[
                ("ANTHROPIC_AUTH_TOKEN", "tok"),
                ("ANTHROPIC_API_KEY", ""),
                ("ANTHROPIC_BASE_URL", "http://127.0.0.1:9/"),
                ("OPENAI_API_KEY", "sk-openai"),
            ]),
        )
        .unwrap();
        assert!(matches!(claude.credential, Credential::Token(_)));
        assert_eq!(url(&claude), "http://127.0.0.1:9/v1/messages");

        let chat = Config::from_vars("gpt", vars(&[("OPENAI_API_KEY", "sk-openai")])).unwrap();
        assert_eq!(url(&chat), "https://api.openai.com/v1/chat/completions");

        let both = [
            ("ANTHROPIC_API_KEY", "sk-claude"),
            ("OPENAI_API_KEY", "sk-openai"),
            ("OPENAI_BASE_URL", "http://127.0.0.1:9/v1"),
            ("MENSOR_API", "chat"),
        ];
        let chosen = Config::from_vars("gpt", vars(&both)).unwrap();
        assert_eq!(url(&chosen), "http://127.0.0.1:9/v1/chat/completions");

        assert!(Config::from_vars("claude-opus-5-5", |_| None).is_err());
        let unknown = [("ANTHROPIC_API_KEY", "sk-claude"), ("MENSOR_API", "soap")];
        assert!(Config::from_vars("claude-opus-5-5", vars(&unknown)).is_err());
        let missing = [("ANTHROPIC_API_KEY", "sk-claude"), ("MENSOR_API", "chat")];
        assert!(Config::from_vars("gpt", vars(&missing)).is_err());
    }

    #[test]
    fn a_busy_api_is_asked_again_after_the_wait_it_names() {
        let config = config();
        let request = request();
        let mut exchange = Exchange::new(&config, &request);
        let busy = Answer::Response {
            status: 529,
            retry_after: Some("7".into()),
            request_id: None,
            body: error("overloaded_error", "busy").to_string(),
        };
        assert_eq!(
            exchange.answered(busy),
            Next::Again {
                wait: Duration::from_secs(7),
                why: ModelError("the model answered 529: overloaded_error: busy".into()),
            }
        );
        let Next::Done(Ok(reply)) = exchange.answered(answer(200, &hello())) else {
            panic!("the call should be over");
        };
        assert_eq!(
            reply.content,
            vec![Block::Text {
                text: "hello".into()
            }]
        );
    }

    #[test]
    fn retries_back_off_and_then_stop() {
        let config = Config {
            max_retries: 2,
            ..config()
        };
        let request = request();
        let mut exchange = Exchange::new(&config, &request);
        let lost = || Answer::Unreachable("connection reset".into());
        let waits: Vec<Duration> = (0..2)
            .map(|_| match exchange.answered(lost()) {
                Next::Again { wait, .. } => wait,
                Next::Done(done) => panic!("stopped early: {done:?}"),
            })
            .collect();
        assert_eq!(waits, [Duration::from_secs(1), Duration::from_secs(2)]);
        assert_eq!(
            exchange.answered(lost()),
            Next::Done(Err(ModelError(
                "the model could not be reached: connection reset".into()
            )))
        );
    }

    #[test]
    fn unbound_reasoning_is_dropped_once() {
        let config = config();
        let request = request();
        let mut exchange = Exchange::new(&config, &request);
        let unbound = || {
            answer(
                400,
                &error(
                    "invalid_request_error",
                    "Invalid `signature` in `thinking` block. The block is bound to a different conversation.",
                ),
            )
        };
        assert!(body_of(&exchange.post()).get("thinking").is_none());
        assert!(matches!(
            exchange.answered(unbound()),
            Next::Again { wait, .. } if wait.is_zero()
        ));

        let post = exchange.post();
        assert_eq!(
            post.header("anthropic-beta"),
            Some(format!("{FALLBACK_BETA},{BINDING_BETA}").as_str())
        );
        assert_eq!(
            body_of(&post)["thinking"]["block_binding"]["prefix_mismatch_behavior"],
            json!("drop_block")
        );
        assert!(matches!(exchange.answered(unbound()), Next::Done(Err(_))));
    }

    #[test]
    fn a_request_the_api_rejects_is_not_sent_again() {
        let config = config();
        let request = request();
        let mut exchange = Exchange::new(&config, &request);
        let rejected = Answer::Response {
            status: 401,
            retry_after: None,
            request_id: Some("req_1".into()),
            body: error("authentication_error", "bad key").to_string(),
        };
        assert_eq!(
            exchange.answered(rejected),
            Next::Done(Err(ModelError(
                "the model answered 401: authentication_error: bad key (request req_1)".into()
            )))
        );
    }

    #[test]
    fn an_error_with_no_type_is_told_by_its_message() {
        let config = Config::chat("sk-test", "gpt");
        let request = request();
        let mut exchange = Exchange::new(&config, &request);
        let rejected = answer(
            400,
            &json!({"error": {"message": "no such model", "code": 400}}),
        );
        assert_eq!(
            exchange.answered(rejected),
            Next::Done(Err(ModelError(
                "the model answered 400: no such model".into()
            )))
        );
    }

    #[test]
    fn an_account_with_nothing_left_is_not_asked_again() {
        let config = Config::chat("sk-test", "gpt");
        let request = request();
        let mut exchange = Exchange::new(&config, &request);
        let spent = answer(
            429,
            &error("insufficient_quota", "You have no credits remaining."),
        );
        assert!(matches!(exchange.answered(spent), Next::Done(Err(_))));
    }

    /// Reads a stream cut into pieces of `size` bytes: what the model
    /// wrote, each piece with where it starts, and how the stream ended.
    fn read(
        config: &Config,
        stream: &str,
        size: usize,
    ) -> (Vec<(u64, String)>, Result<String, String>) {
        let request = request();
        let mut reading = Exchange::new(config, &request).reading();
        let mut written = Vec::new();
        for bytes in stream.as_bytes().chunks(size) {
            reading.read(bytes, &mut |at, text| written.push((at, text.to_owned())));
        }
        (written, reading.finish())
    }

    /// Server-sent events of these data.
    fn events(data: &[Value]) -> String {
        let events = data
            .iter()
            .map(|data| format!("event: x\ndata: {data}\n\n"));
        events.collect()
    }

    #[test]
    fn a_streamed_message_is_read_as_one_that_was_not() {
        let stream = events(&[
            json!({"type": "message_start", "message": {
                "role": "assistant", "content": [], "stop_reason": null,
                "usage": {"input_tokens": 10, "cache_read_input_tokens": 90, "output_tokens": 1},
            }}),
            json!({"type": "ping"}),
            json!({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "thinking_delta", "thinking": "Hm"}}),
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "signature_delta", "signature": "sig"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1,
                   "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 1,
                   "delta": {"type": "text_delta", "text": "Loo"}}),
            json!({"type": "content_block_delta", "index": 1,
                   "delta": {"type": "text_delta", "text": "king é."}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "content_block_start", "index": 2,
                   "content_block": {"type": "tool_use", "id": "tu_1", "name": "bash", "input": {}}}),
            json!({"type": "content_block_delta", "index": 2,
                   "delta": {"type": "input_json_delta", "partial_json": "{\"comm"}}),
            json!({"type": "content_block_delta", "index": 2,
                   "delta": {"type": "input_json_delta", "partial_json": "and\":\"ls\"}"}}),
            json!({"type": "content_block_stop", "index": 2}),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"},
                   "usage": {"output_tokens": 7}}),
            json!({"type": "message_stop"}),
        ]);
        let whole = json!({
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "Hm", "signature": "sig"},
                {"type": "text", "text": "Looking é."},
                {"type": "tool_use", "id": "tu_1", "name": "bash", "input": {"command": "ls"}},
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "cache_read_input_tokens": 90, "output_tokens": 7},
        });

        // However the bytes are cut, even through a character.
        for size in [1, 7, 4096] {
            let (written, body) = read(&config(), &stream, size);
            let body = body.unwrap();
            assert_eq!(serde_json::from_str::<Value>(&body).unwrap(), whole);
            let text: String = written.iter().map(|(_, text)| text.as_str()).collect();
            assert_eq!(text, "Looking é.");
            let mut at = 0;
            for (starts, text) in &written {
                assert_eq!(*starts, at);
                at += text.len() as u64;
            }
        }

        let mut exchange_request = request();
        exchange_request.messages.truncate(1);
        let config = config();
        let mut exchange = Exchange::new(&config, &exchange_request);
        let (_, body) = read(&config, &stream, 4096);
        let Next::Done(Ok(reply)) = exchange.answered(answer_of(body.unwrap())) else {
            panic!("the call should be over");
        };
        assert_eq!(reply.content.len(), 3);
        assert_eq!(reply.usage.input, 100);
    }

    fn answer_of(body: String) -> Answer {
        Answer::Response {
            status: 200,
            retry_after: None,
            request_id: None,
            body,
        }
    }

    #[test]
    fn a_streamed_completion_is_read_as_one_that_was_not() {
        let delta = |delta: Value| json!({"choices": [{"index": 0, "delta": delta}]});
        let call = |index: u64, call: Value| {
            let mut call = call;
            call["index"] = json!(index);
            delta(json!({"tool_calls": [call]}))
        };
        let mut stream = String::from(": processing\n\n");
        stream.push_str(&events(&[
            delta(
                json!({"role": "assistant", "content": "", "reasoning_details": [
                    {"type": "reasoning.text", "text": "Let me ", "signature": null, "index": 0},
                ]}),
            ),
            delta(json!({"reasoning_details": [
                {"type": "reasoning.text", "text": "see.", "signature": "sig", "index": 0},
            ]})),
            delta(json!({"reasoning_details": [
                {"type": "reasoning.encrypted", "data": "abc", "index": 1},
            ]})),
            delta(json!({"content": "Loo"})),
            delta(json!({"content": "king."})),
            call(
                0,
                json!({"id": "call_1", "type": "function",
                           "function": {"name": "bash", "arguments": ""}}),
            ),
            call(0, json!({"function": {"arguments": "{\"command\":"}})),
            call(0, json!({"function": {"arguments": "\"ls\"}"}})),
            call(
                1,
                json!({"id": "call_2", "type": "function",
                           "function": {"name": "list", "arguments": "{}"}}),
            ),
            // A server that numbers every call 0 gives each its own id.
            call(
                0,
                json!({"id": "call_3", "type": "function",
                           "function": {"name": "bash", "arguments": "{}"}}),
            ),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 100, "completion_tokens": 7}}),
        ]));
        stream.push_str("data: [DONE]\n\n");

        let config = Config::chat("sk-test", "gpt");
        let (written, body) = read(&config, &stream, 5);
        let text: String = written.iter().map(|(_, text)| text.as_str()).collect();
        assert_eq!(text, "Looking.");

        let request = request();
        let mut exchange = Exchange::new(&config, &request);
        let Next::Done(Ok(reply)) = exchange.answered(answer_of(body.unwrap())) else {
            panic!("the call should be over");
        };
        let tool_use = |id: &str, name: &str, input: Value| Block::ToolUse {
            id: id.into(),
            name: name.into(),
            input,
        };
        assert_eq!(
            reply.content,
            vec![
                Block::Other {
                    value: json!({"type": "reasoning_details", "details": [
                        {"type": "reasoning.text", "text": "Let me see.", "signature": "sig", "index": 0},
                        {"type": "reasoning.encrypted", "data": "abc", "index": 1},
                    ]})
                },
                Block::Text {
                    text: "Looking.".into()
                },
                tool_use("call_1", "bash", json!({"command": "ls"})),
                tool_use("call_2", "list", json!({})),
                tool_use("call_3", "bash", json!({})),
            ]
        );
        assert_eq!(reply.stop, mensor_mind::Stop::ToolUse);
        assert_eq!((reply.usage.input, reply.usage.output), (100, 7));
    }

    #[test]
    fn a_stream_that_fails_or_stops_short_is_no_reply() {
        let begun = json!({"type": "message_start", "message": {"content": []}});
        let (_, cut) = read(&config(), &events(std::slice::from_ref(&begun)), 4096);
        assert_eq!(cut, Err(CUT_OFF.into()));

        let busy = json!({"type": "error",
                          "error": {"type": "overloaded_error", "message": "Overloaded"}});
        let (_, failed) = read(&config(), &events(&[begun, busy]), 4096);
        assert_eq!(failed, Err("overloaded_error: Overloaded".into()));

        let chat = Config::chat("sk-test", "gpt");
        let said = json!({"choices": [{"delta": {"content": "Good"}}]});
        let (written, cut) = read(&chat, &events(std::slice::from_ref(&said)), 4096);
        assert_eq!(written, [(0, "Good".to_owned())]);
        assert_eq!(cut, Err(CUT_OFF.into()));

        let failed = json!({"error": {"code": 502, "message": "the provider failed"},
                            "choices": [{"delta": {"content": ""}, "finish_reason": "error"}]});
        let (_, failed) = read(&chat, &events(&[said, failed]), 4096);
        assert_eq!(failed, Err("the provider failed".into()));

        // The lost reply is asked for again.
        let (config, request) = (config(), request());
        let mut exchange = Exchange::new(&config, &request);
        let lost = Answer::Unreachable(CUT_OFF.into());
        assert!(matches!(exchange.answered(lost), Next::Again { .. }));
    }

    #[test]
    fn a_reply_that_is_not_json_ends_the_call() {
        for config in [config(), Config::chat("sk-test", "gpt")] {
            let request = request();
            let mut exchange = Exchange::new(&config, &request);
            let garbled = Answer::Response {
                status: 200,
                retry_after: None,
                request_id: None,
                body: "<html>".into(),
            };
            assert!(matches!(exchange.answered(garbled), Next::Done(Err(_))));
        }
    }
}

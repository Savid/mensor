//! A model behind an HTTP API.
//!
//! [`mensor_model`] decides what to send and what an answer means. This is
//! the sending: one HTTP request per attempt, read as it streams, with the
//! waits it asks for.

use std::time::{Duration, Instant};

use mensor_mind::{ModelReply, ModelRequest};
use mensor_model::{Answer, Config, Exchange, Next};
use reqwest::Client;
use reqwest::header::{CONTENT_TYPE, HeaderValue, RETRY_AFTER};

use crate::model::{Model, ModelError, ModelFuture, Wrote};

/// A model reached over HTTP, as a [`Model`].
#[derive(Clone, Debug)]
pub struct Provider {
    http: Client,
    config: Config,
}

impl Provider {
    /// A client for the API `config` names.
    pub fn new(config: Config) -> Result<Self, ModelError> {
        // The cryptography TLS uses. The first choice in a process stands,
        // so a hand in this process and its mind's host share one.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|error| ModelError(format!("the HTTP client could not be made: {error}")))?;
        Ok(Self { http, config })
    }

    async fn ask(
        &self,
        request: &ModelRequest,
        wrote: &mut Wrote,
    ) -> Result<ModelReply, ModelError> {
        let mut exchange = Exchange::new(&self.config, request);
        loop {
            let (answer, (first, took)) = self.send(&exchange, wrote).await?;
            match exchange.answered(answer) {
                Next::Done(done) => {
                    return done.map(|mut reply| {
                        let ms = |time: Duration| u64::try_from(time.as_millis()).ok();
                        reply.usage.first_token_ms = first.and_then(ms);
                        reply.usage.took_ms = ms(took);
                        reply
                    });
                }
                Next::Again { wait, why } => {
                    tracing::warn!(error = %why, ?wait, "the model request failed: trying again");
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }

    /// Sends one request and reads what comes back: a stream of events as
    /// it arrives, and anything else whole. Says too how long the model
    /// took to begin its streamed answer, and how long the request took.
    async fn send(
        &self,
        exchange: &Exchange<'_>,
        wrote: &mut Wrote,
    ) -> Result<(Answer, (Option<Duration>, Duration)), ModelError> {
        let mut first = None;
        let post = exchange.post();
        let mut sending = self
            .http
            .post(&post.url)
            .header(CONTENT_TYPE, "application/json")
            .body(post.body);
        for (name, value) in &post.headers {
            let mut value = HeaderValue::from_str(value)
                .map_err(|_| ModelError(format!("{name} is not a valid header")))?;
            value.set_sensitive(true);
            sending = sending.header(*name, value);
        }
        let lost = |error: reqwest::Error| error.without_url().to_string();
        let sent = Instant::now();
        let mut response = match sending.send().await {
            Ok(response) => response,
            Err(error) => return Ok((Answer::Unreachable(lost(error)), (None, sent.elapsed()))),
        };
        let status = response.status();
        let header = |name: &str| {
            let value = response.headers().get(name)?.to_str().ok()?;
            Some(value.to_owned())
        };
        let retry_after = header(RETRY_AFTER.as_str());
        let request_id = header("request-id").or_else(|| header("x-request-id"));
        let kind = header(CONTENT_TYPE.as_str()).unwrap_or_default();
        let body = if status.is_success() && kind.starts_with("text/event-stream") {
            let mut reading = exchange.reading();
            loop {
                match response.chunk().await {
                    Ok(Some(bytes)) => {
                        reading.read(&bytes, wrote);
                        if reading.began() {
                            first.get_or_insert_with(|| sent.elapsed());
                        }
                    }
                    Ok(None) => break reading.finish(),
                    Err(error) => break Err(lost(error)),
                }
            }
        } else {
            response.text().await.map_err(lost)
        };
        let answer = match body {
            Ok(body) => Answer::Response {
                status: status.as_u16(),
                retry_after,
                request_id,
                body,
            },
            Err(lost) => Answer::Unreachable(lost),
        };
        Ok((answer, (first, sent.elapsed())))
    }
}

impl Model for Provider {
    fn call(&self, request: ModelRequest, mut wrote: Wrote) -> ModelFuture<'_> {
        Box::pin(async move { self.ask(&request, &mut wrote).await })
    }

    fn name(&self) -> &str {
        &self.config.model
    }

    fn efforts(&self) -> Option<&'static [&'static str]> {
        self.config.api.efforts()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use axum::Json;
    use axum::extract::State;
    use axum::http::{HeaderMap as Headers, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::post;
    use mensor_mind::{Block, Content, Message, Role};
    use mensor_model::{BINDING_BETA, Credential, FALLBACK_BETA, VERSION};
    use serde_json::{Value, json};
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;

    use super::*;

    fn config() -> Config {
        Config::messages(Credential::ApiKey("sk-test".into()), "claude-opus-5-5")
    }

    fn request() -> ModelRequest {
        ModelRequest {
            seen: 1,
            model: None,
            effort: None,
            system: Some("Be brief.".into()),
            messages: vec![Message {
                role: Role::User,
                content: vec![Content::Text("say hello".into())],
            }],
            tools: Vec::new(),
            text_only: false,
        }
    }

    /// The requests an API was sent.
    type Seen = Arc<Mutex<Vec<(Headers, Value)>>>;

    #[derive(Clone, Default)]
    struct Api {
        calls: Arc<AtomicU32>,
        seen: Seen,
    }

    /// An API that is overloaded once, then refuses bound reasoning once,
    /// then answers.
    async fn messages(
        State(api): State<Api>,
        headers: Headers,
        Json(body): Json<Value>,
    ) -> axum::response::Response {
        api.seen.lock().await.push((headers, body));
        match api.calls.fetch_add(1, Ordering::SeqCst) {
            0 => (
                StatusCode::from_u16(529).unwrap(),
                [("retry-after", "0")],
                Json(json!({"type": "error", "error": {"type": "overloaded_error", "message": "busy"}})),
            )
                .into_response(),
            1 => (
                StatusCode::BAD_REQUEST,
                Json(json!({"type": "error", "error": {
                    "type": "invalid_request_error",
                    "message": "Invalid `signature` in `thinking` block. The block is bound to a different conversation.",
                }})),
            )
                .into_response(),
            _ => Json(json!({
                "content": [{"type": "text", "text": "hello"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 1, "output_tokens": 1},
            }))
            .into_response(),
        }
    }

    /// Serves `router` on a port of its own, and gives its address.
    async fn serve(router: axum::Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn a_busy_api_is_asked_again_and_unbound_reasoning_is_dropped_once() {
        let api = Api::default();
        let router = axum::Router::new()
            .route("/v1/messages", post(messages))
            .with_state(api.clone());
        let base_url = serve(router).await;
        let model = Provider::new(Config {
            base_url,
            ..config()
        })
        .unwrap();
        let reply = model.call(request(), Box::new(|_, _| {})).await.unwrap();
        assert_eq!(
            reply.content,
            vec![Block::Text {
                text: "hello".into()
            }]
        );

        let seen = api.seen.lock().await;
        assert_eq!(seen.len(), 3);
        let (headers, body) = &seen[0];
        assert_eq!(headers["content-type"], "application/json");
        assert_eq!(headers["x-api-key"], "sk-test");
        assert_eq!(headers["anthropic-version"], VERSION);
        assert_eq!(headers["anthropic-beta"], FALLBACK_BETA);
        assert_eq!(
            body["thinking"],
            json!({"type": "adaptive", "display": "summarized"})
        );

        let (headers, body) = &seen[2];
        assert_eq!(
            headers["anthropic-beta"],
            format!("{FALLBACK_BETA},{BINDING_BETA}").as_str()
        );
        assert_eq!(
            body["thinking"]["block_binding"]["prefix_mismatch_behavior"],
            json!("drop_block")
        );
    }

    #[tokio::test]
    async fn a_reply_that_streams_is_told_as_it_comes() {
        let events = [
            json!({"type": "message_start", "message": {
                "content": [], "usage": {"input_tokens": 9, "output_tokens": 1},
            }}),
            json!({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "text_delta", "text": "good "}}),
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "text_delta", "text": "day"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
                   "usage": {"output_tokens": 2}}),
            json!({"type": "message_stop"}),
        ];
        let events = events.map(|event| format!("event: {}\ndata: {event}\n\n", event["type"]));
        let stream = events.concat();
        let router = axum::Router::new().route(
            "/v1/messages",
            post(|Json(body): Json<Value>| async move {
                assert_eq!(body["stream"], json!(true));
                ([("content-type", "text/event-stream")], stream)
            }),
        );
        let model = Provider::new(Config {
            base_url: serve(router).await,
            ..config()
        })
        .unwrap();

        let written = Arc::new(std::sync::Mutex::new(String::new()));
        let wrote = {
            let written = Arc::clone(&written);
            move |at: u64, text: &str| {
                let mut written = written.lock().unwrap();
                assert_eq!(at, written.len() as u64);
                written.push_str(text);
            }
        };
        let reply = model.call(request(), Box::new(wrote)).await.unwrap();
        assert_eq!(*written.lock().unwrap(), "good day");
        assert_eq!(
            reply.content,
            vec![Block::Text {
                text: "good day".into()
            }]
        );
        assert_eq!((reply.usage.input, reply.usage.output), (9, 2));
        assert!(reply.usage.first_token_ms.is_some());
    }

    #[tokio::test]
    async fn a_rejected_request_is_told_with_its_status_type_message_and_request_id() {
        let router = axum::Router::new().route(
            "/v1/messages",
            post(|| async {
                (
                    StatusCode::UNAUTHORIZED,
                    [("request-id", "req_1")],
                    Json(json!({"type": "error", "error": {"type": "authentication_error", "message": "bad key"}})),
                )
            }),
        );
        let model = Provider::new(Config {
            base_url: serve(router).await,
            ..config()
        })
        .unwrap();
        let error = model
            .call(request(), Box::new(|_, _| {}))
            .await
            .unwrap_err();
        assert_eq!(
            error.0,
            "the model answered 401: authentication_error: bad key (request req_1)"
        );
    }

    #[tokio::test]
    async fn a_model_behind_chat_completions_is_asked_in_its_own_words() {
        let seen = Seen::default();
        let router = axum::Router::new()
            .route(
                "/v1/chat/completions",
                post(
                    |State(seen): State<Seen>,
                     headers: Headers,
                     Json(body): Json<Value>| async move {
                        seen.lock().await.push((headers, body));
                        Json(json!({
                            "choices": [{
                                "message": {"role": "assistant", "content": "hello"},
                                "finish_reason": "stop",
                            }],
                            "usage": {"prompt_tokens": 9, "completion_tokens": 1},
                        }))
                    },
                ),
            )
            .with_state(Arc::clone(&seen));
        let model = Provider::new(Config {
            base_url: format!("{}/v1", serve(router).await),
            ..Config::chat("sk-test", "gpt")
        })
        .unwrap();
        let reply = model.call(request(), Box::new(|_, _| {})).await.unwrap();
        assert_eq!(
            reply.content,
            vec![Block::Text {
                text: "hello".into()
            }]
        );
        assert_eq!(reply.usage.input, 9);

        let seen = seen.lock().await;
        let (headers, body) = &seen[0];
        assert_eq!(headers["authorization"], "Bearer sk-test");
        assert_eq!(
            body["messages"],
            json!([
                {"role": "system", "content": "Be brief."},
                {"role": "user", "content": "say hello"},
            ])
        );
    }
}

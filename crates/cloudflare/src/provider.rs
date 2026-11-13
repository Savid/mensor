//! A model, asked with the platform's `fetch`.
//!
//! [`mensor_model`] decides what to send and what an answer means. This is
//! the sending, and the reading of an answer as it streams.

use std::time::Duration;

use futures_util::StreamExt;
use futures_util::future::{Either, select};
use mensor_mind::{ModelError, ModelReply, ModelRequest};
use mensor_model::{Answer, Config, Exchange, Next};
use mensor_object::Model;
use worker::wasm_bindgen::JsValue;
use worker::{
    AbortController, Delay, Env, Fetch, Headers, Method, Request, RequestInit, console_warn,
};

/// How long one request to the model may take. It is shorter than a
/// server allows, because the platform bounds how long an object's alarm
/// runs.
pub(crate) const TIMEOUT: Duration = Duration::from_mins(4);

/// A model behind an HTTP API, or the reason it cannot be asked.
pub(crate) struct Provider<W> {
    config: Result<Config, ModelError>,
    /// Told the text of a reply as the model writes it: how many bytes of
    /// the text came before, and the next piece.
    wrote: W,
}

impl<W: Fn(u64, &str)> Provider<W> {
    /// The model the object's variables and secrets describe.
    pub(crate) fn from_env(env: &Env, wrote: W) -> Self {
        let var = |name: &str| env.var(name).ok().map(|value| value.to_string());
        let model = var("MENSOR_MODEL").unwrap_or_else(|| "claude-opus-5-5".into());
        let config = Config::from_vars(model, var).map(|mut config| {
            config.effort = var("MENSOR_EFFORT").filter(|effort| !effort.is_empty());
            config.fallbacks = var("MENSOR_FALLBACKS").as_deref() != Some("off");
            config.timeout = TIMEOUT;
            config
        });
        Self { config, wrote }
    }
}

impl<W: Fn(u64, &str)> Model for Provider<W> {
    async fn ask(&self, request: &ModelRequest) -> Result<ModelReply, ModelError> {
        let config = self.config.as_ref().map_err(Clone::clone)?;
        let mut exchange = Exchange::new(config, request);
        // A summary is no reply, so nobody is told it is being written.
        let mut wrote = |at: u64, text: &str| {
            if !request.text_only {
                (self.wrote)(at, text);
            }
        };
        loop {
            let answer = send(&exchange, config.timeout, &mut wrote).await;
            match exchange.answered(answer) {
                Next::Done(done) => return done,
                Next::Again { wait, why } => {
                    console_warn!("the model request failed, trying again: {why}");
                    Delay::from(wait).await;
                }
            }
        }
    }
}

/// Sends one request and reads what comes back, a stream of events as it
/// arrives and anything else whole. It gives up after `timeout`.
async fn send(
    exchange: &Exchange<'_>,
    timeout: Duration,
    wrote: &mut dyn FnMut(u64, &str),
) -> Answer {
    let post = exchange.post();
    let controller = AbortController::default();
    let signal = controller.signal();
    let answered = std::pin::pin!(async {
        let headers = Headers::new();
        headers.set("content-type", "application/json")?;
        for (name, value) in &post.headers {
            headers.set(name, value)?;
        }
        let mut init = RequestInit::new();
        init.with_method(Method::Post)
            .with_headers(headers)
            .with_body(Some(JsValue::from_str(&post.body)));
        let request = Request::new_with_init(&post.url, &init)?;
        let mut response = Fetch::Request(request).send_with_signal(&signal).await?;
        let status = response.status_code();
        let retry_after = response.headers().get("retry-after")?;
        let request_id = match response.headers().get("request-id")? {
            Some(id) => Some(id),
            None => response.headers().get("x-request-id")?,
        };
        let kind = response.headers().get("content-type")?.unwrap_or_default();
        let body = if (200..300).contains(&status) && kind.starts_with("text/event-stream") {
            let mut reading = exchange.reading();
            let mut stream = std::pin::pin!(response.stream()?);
            while let Some(bytes) = stream.next().await {
                reading.read(&bytes?, wrote);
            }
            match reading.finish() {
                Ok(body) => body,
                Err(lost) => return Ok(Answer::Unreachable(lost)),
            }
        } else {
            response.text().await?
        };
        Ok::<_, worker::Error>(Answer::Response {
            status,
            retry_after,
            request_id,
            body,
        })
    });
    let waited = std::pin::pin!(Delay::from(timeout));
    match select(answered, waited).await {
        Either::Left((Ok(answer), _)) => answer,
        Either::Left((Err(error), _)) => Answer::Unreachable(error.to_string()),
        Either::Right(((), _)) => {
            controller.abort();
            Answer::Unreachable(format!("no answer in {} seconds", timeout.as_secs()))
        }
    }
}

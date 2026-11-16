//! The `fetch` capability: an HTTP request made by the platform's `fetch`.

use std::collections::BTreeMap;
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::future::{Either, select};
use mensor_hand::{CallFuture, Capability};
use mensor_protocol::{CapabilityDecl, OperationDecl, Reply};
use serde::Deserialize;
use serde_json::{Value, json};
use worker::send::SendFuture;
use worker::wasm_bindgen::JsValue;
use worker::{AbortController, Delay, Fetch, Headers, Method, Request, RequestInit};

/// The `fetch` capability. Its one operation is `fetch`.
///
/// It reaches whatever the platform lets a worker reach, which is the
/// public internet. Narrowing that is left to whoever deploys the worker.
#[derive(Clone, Debug)]
pub(crate) struct Web {
    /// The most bytes of a response's body that are kept.
    pub(crate) max_body: usize,
    /// How long one request may take.
    pub(crate) timeout: Duration,
}

impl Default for Web {
    fn default() -> Self {
        Self {
            max_body: 256 * 1024,
            timeout: Duration::from_mins(1),
        }
    }
}

#[derive(Deserialize)]
struct Input {
    url: String,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    body: Option<String>,
}

impl Capability for Web {
    fn declare(&self) -> CapabilityDecl {
        CapabilityDecl {
            name: "fetch".into(),
            operations: vec![OperationDecl {
                name: "fetch".into(),
                description:
                    "Make an HTTP request, and return its status and the start of its body.".into(),
                input: json!({
                    "type": "object",
                    "properties": {
                        "url": {"type": "string", "description": "An http or https address."},
                        "method": {"type": "string", "description": "GET unless given."},
                        "headers": {
                            "type": "object",
                            "description": "Request headers, by name.",
                            "additionalProperties": {"type": "string"},
                        },
                        "body": {"type": "string", "description": "The request's body."},
                    },
                    "required": ["url"],
                }),
            }],
        }
    }

    fn call(&self, operation: &str, input: Value) -> CallFuture<'_> {
        let operation = operation.to_owned();
        Box::pin(SendFuture::new(async move {
            if operation != "fetch" {
                return Reply::Error(format!("fetch has no operation named {operation}"));
            }
            let input: Input = match mensor_hand::input(&operation, input) {
                Ok(input) => input,
                Err(reply) => return reply,
            };
            match self.fetch(input).await {
                Ok(output) => Reply::Output(output),
                Err(error) => Reply::Error(error),
            }
        }))
    }
}

impl Web {
    async fn fetch(&self, input: Input) -> Result<Value, String> {
        if !(input.url.starts_with("https://") || input.url.starts_with("http://")) {
            return Err("the url must start with http:// or https://".into());
        }
        let method = input
            .method
            .as_deref()
            .unwrap_or("GET")
            .to_ascii_uppercase();
        if !matches!(
            method.as_str(),
            "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE"
        ) {
            return Err(format!("fetch does not send {method} requests"));
        }

        let controller = AbortController::default();
        let signal = controller.signal();
        let url = input.url.clone();
        let answered = std::pin::pin!(async {
            let headers = Headers::new();
            for (name, value) in &input.headers {
                headers.set(name, value)?;
            }
            let mut init = RequestInit::new();
            init.with_method(Method::from(method))
                .with_headers(headers)
                .with_body(input.body.as_deref().map(JsValue::from_str));
            let request = Request::new_with_init(&input.url, &init)?;
            let mut response = Fetch::Request(request).send_with_signal(&signal).await?;
            let status = response.status_code();
            let content_type = response.headers().get("content-type")?;

            // Only the start of a body is kept, so the rest is never read.
            let mut body = Vec::new();
            let mut truncated = false;
            let mut chunks = response.stream()?;
            while let Some(chunk) = chunks.next().await {
                body.extend_from_slice(&chunk?);
                if body.len() > self.max_body {
                    body.truncate(self.max_body);
                    truncated = true;
                    break;
                }
            }
            Ok::<_, worker::Error>(json!({
                "status": status,
                "contentType": content_type,
                "body": String::from_utf8_lossy(&body),
                "truncated": truncated,
            }))
        });
        let waited = std::pin::pin!(Delay::from(self.timeout));
        match select(answered, waited).await {
            Either::Left((Ok(output), _)) => Ok(output),
            Either::Left((Err(error), _)) => Err(format!("{url} could not be fetched: {error}")),
            Either::Right(((), _)) => {
                controller.abort();
                Err(format!(
                    "{url} did not answer in {} seconds",
                    self.timeout.as_secs()
                ))
            }
        }
    }
}

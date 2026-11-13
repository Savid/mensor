use std::future::Future;
use std::pin::Pin;
use std::sync::Once;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use mensor_protocol::{FromBody, FromMind, Hello, MAX_FRAME_BYTES, Refusal};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::protocol::{Message, WebSocketConfig};
use tokio_tungstenite::tungstenite::{self};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async_with_config};

use crate::Error;

/// A future a transport returns.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A way for a body to reach its mind.
///
/// The protocol names no transport, so a body is given one. [`WebSocket`]
/// dials a host over the network. A host in the same process hands the body
/// a transport that passes messages in memory.
pub trait Dial: Send + Sync {
    /// Opens a connection and says `hello` on it.
    fn dial(&self, hello: Hello) -> BoxFuture<'_, Result<Box<dyn Link>, DialError>>;
}

/// One connection to a mind. Dropping it ends the connection.
pub trait Link: Send {
    /// Sends one message.
    fn send(&mut self, message: FromBody) -> BoxFuture<'_, Result<(), Gone>>;

    /// Waits for what the mind sends next. A caller may drop this future and
    /// call again without losing a message.
    fn receive(&mut self) -> BoxFuture<'_, Incoming>;

    /// Shows a quiet connection is still alive, where the transport needs it.
    fn ping(&mut self) -> BoxFuture<'_, Result<(), Gone>> {
        Box::pin(async { Ok(()) })
    }

    /// Ends the connection in the transport's own polite way.
    fn close(self: Box<Self>) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}

/// What arrives on a connection.
#[derive(Debug)]
pub enum Incoming {
    /// A message from the mind.
    Message(FromMind),
    /// The host ended the connection, for this reason.
    Refused(Refusal),
    /// The connection is gone.
    Closed,
}

/// The connection is gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gone;

/// Why a dial did not connect.
#[derive(Debug)]
pub enum DialError {
    /// Dialling again may work.
    Retry(String),
    /// The host turned the body away.
    Refused(Refusal),
    /// Dialling again cannot work.
    Fail(Error),
}

/// Dials a mind's host over a WebSocket.
#[derive(Clone)]
pub struct WebSocket {
    url: String,
    token: Option<String>,
    /// How long a dial may take.
    pub connect_timeout: Duration,
}

impl WebSocket {
    /// A dialler for `url`, such as `ws://127.0.0.1:7878/v1/attach`. A
    /// `wss://` address is dialled over TLS, trusting the system's roots.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            token: None,
            connect_timeout: Duration::from_secs(10),
        }
    }

    /// Sends this bearer token with every dial, for a host that requires one.
    #[must_use]
    pub fn token(mut self, token: Option<String>) -> Self {
        self.token = token;
        self
    }
}

impl std::fmt::Debug for WebSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSocket")
            .field("url", &self.url)
            .field("token", &self.token.as_ref().map(|_| ".."))
            .finish_non_exhaustive()
    }
}

/// Picks the cryptography a `wss://` dial uses. The first call in a process
/// decides, so a host that has already chosen keeps its choice.
fn tls() {
    static CHOSEN: Once = Once::new();
    CHOSEN.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

impl Dial for WebSocket {
    fn dial(&self, hello: Hello) -> BoxFuture<'_, Result<Box<dyn Link>, DialError>> {
        Box::pin(async move {
            tls();
            let unusable =
                |error: &dyn std::fmt::Display| DialError::Fail(Error::Config(error.to_string()));
            let mut request = self
                .url
                .as_str()
                .into_client_request()
                .map_err(|error| unusable(&error))?;
            if let Some(token) = &self.token {
                let bearer = HeaderValue::from_str(&format!("Bearer {token}"))
                    .map_err(|error| unusable(&error))?;
                request.headers_mut().insert("authorization", bearer);
            }
            let limits = WebSocketConfig::default()
                .max_message_size(Some(MAX_FRAME_BYTES))
                .max_frame_size(Some(MAX_FRAME_BYTES));
            let connecting = connect_async_with_config(request, Some(limits), false);
            let mut socket = match timeout(self.connect_timeout, connecting).await {
                Ok(Ok((socket, _))) => socket,
                Ok(Err(tungstenite::Error::Http(response)))
                    if matches!(
                        response.status(),
                        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
                    ) =>
                {
                    return Err(DialError::Fail(Error::Unauthorized));
                }
                Ok(Err(tungstenite::Error::Url(error))) => return Err(unusable(&error)),
                Ok(Err(error)) => return Err(DialError::Retry(error.to_string())),
                Err(_) => return Err(DialError::Retry("the dial timed out".into())),
            };
            let hello = FromBody::Hello(Box::new(hello)).encode();
            socket
                .send(Message::text(hello))
                .await
                .map_err(|error| DialError::Retry(error.to_string()))?;
            Ok(Box::new(Socket(socket)) as Box<dyn Link>)
        })
    }
}

struct Socket(WebSocketStream<MaybeTlsStream<TcpStream>>);

impl Link for Socket {
    fn send(&mut self, message: FromBody) -> BoxFuture<'_, Result<(), Gone>> {
        Box::pin(async move {
            self.0
                .send(Message::text(message.encode()))
                .await
                .map_err(|_| Gone)
        })
    }

    fn receive(&mut self) -> BoxFuture<'_, Incoming> {
        Box::pin(async move {
            loop {
                match self.0.next().await {
                    Some(Ok(Message::Text(text))) => {
                        return match FromMind::decode(text.as_str()) {
                            Ok(message) => Incoming::Message(message),
                            Err(error) => {
                                tracing::warn!(%error, "the mind sent what the protocol does not define");
                                Incoming::Closed
                            }
                        };
                    }
                    Some(Ok(Message::Close(frame))) => {
                        let refusal = frame.and_then(|frame| Refusal::from_code(frame.code.into()));
                        return refusal.map_or(Incoming::Closed, Incoming::Refused);
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => return Incoming::Closed,
                }
            }
        })
    }

    fn ping(&mut self) -> BoxFuture<'_, Result<(), Gone>> {
        Box::pin(async move {
            self.0
                .send(Message::Ping(Vec::new().into()))
                .await
                .map_err(|_| Gone)
        })
    }

    fn close(mut self: Box<Self>) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            let _ = self.0.close(None).await;
        })
    }
}

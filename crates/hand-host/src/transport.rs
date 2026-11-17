use std::future::Future;
use std::pin::Pin;
use std::sync::Once;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use mensor_protocol::{FromHand, FromMind, Hello, MAX_FRAME_BYTES, NOT_LOCAL, Refusal};
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

/// A way for a hand to reach its mind.
///
/// The protocol names no transport, so a hand is given one. [`WebSocket`]
/// dials a host over the network. A host in the same process hands the hand
/// a transport that passes messages in memory.
pub trait Dial: Send + Sync {
    /// Opens a connection and says `hello` on it.
    fn dial(&self, hello: Hello) -> BoxFuture<'_, Result<Box<dyn Link>, DialError>>;
}

/// One connection to a mind. Dropping it ends the connection.
pub trait Link: Send {
    /// Sends one message.
    fn send(&mut self, message: FromHand) -> BoxFuture<'_, Result<(), Gone>>;

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
#[derive(Debug, thiserror::Error)]
pub enum DialError {
    /// Dialling again may work.
    #[error("{0}")]
    Retry(String),
    /// The host turned the hand away.
    #[error("the host refused: {}", .0.reason())]
    Refused(Refusal),
    /// Dialling again cannot work.
    #[error(transparent)]
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

/// The start of what a refusal's body says, on one line. A host's problem
/// holds its `detail` word for word, since none needs escaping in JSON; a
/// proxy's says what it says.
fn said(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let words: Vec<&str> = text.split_whitespace().collect();
    words.join(" ").chars().take(200).collect()
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
                    if response.status() == StatusCode::UNAUTHORIZED =>
                {
                    return Err(DialError::Fail(Error::Unauthorized));
                }
                Ok(Err(tungstenite::Error::Http(response)))
                    if response.status() == StatusCode::FORBIDDEN =>
                {
                    let said = response.body().as_deref().map(said).unwrap_or_default();
                    return Err(DialError::Fail(if said.contains(NOT_LOCAL) {
                        Error::NotLocal
                    } else if said.is_empty() {
                        Error::Forbidden("no reason given".into())
                    } else {
                        Error::Forbidden(said)
                    }));
                }
                Ok(Err(tungstenite::Error::Url(error))) => return Err(unusable(&error)),
                Ok(Err(error)) => return Err(DialError::Retry(error.to_string())),
                Err(_) => return Err(DialError::Retry("the dial timed out".into())),
            };
            let hello = FromHand::Hello(Box::new(hello)).encode();
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
    fn send(&mut self, message: FromHand) -> BoxFuture<'_, Result<(), Gone>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_logged_through_the_log_crate() {
        // tungstenite logs each handshake there, `Authorization` included.
        assert_eq!(log::STATIC_MAX_LEVEL, log::LevelFilter::Off);
    }

    #[test]
    fn a_refusal_is_told_as_its_body_says_it_on_one_line() {
        let body = format!(r#"{{"status":403,"detail":"{NOT_LOCAL}"}}"#);
        assert!(said(body.as_bytes()).contains(NOT_LOCAL));
        assert_eq!(
            said(b"<html>\n  Forbidden\n</html>"),
            "<html> Forbidden </html>"
        );
        assert_eq!(said(&[b'x'; 500]).len(), 200);
    }
}

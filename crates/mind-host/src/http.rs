use std::collections::VecDeque;
use std::convert::Infallible;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, Request, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream::{self, Stream};
use futures_util::{SinkExt, StreamExt};
use mensor_api::{
    History, MindStatus, PAGE, Problem, PromptBody, PromptReply, Writing, authorized,
};
use mensor_mind::{Item, StoreError};
use mensor_protocol::{FromHand, Hello, MAX_FRAME_BYTES, Name, Refusal, VERSION, version_of};
use serde::Deserialize;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::timeout;

use crate::host::{Host, HostError, Rejected};
use crate::task::{Outgoing, Told};

impl Host {
    /// The attach endpoint and the API as an [`axum`] router.
    pub fn router(&self) -> Router {
        Router::new()
            .route("/v1/attach", get(attach))
            .route("/v1/minds", get(pairs))
            .route("/v1/minds/{name}", get(pair))
            .route("/v1/minds/{name}/prompt", post(prompt))
            .route("/v1/minds/{name}/interrupt", post(interrupt))
            .route("/v1/minds/{name}/history", get(history))
            .route("/v1/minds/{name}/events", get(events))
            .layer(middleware::from_fn_with_state(self.clone(), guard))
            .with_state(self.clone())
    }
}

fn problem(status: StatusCode, detail: impl Into<String>) -> Response {
    let body = Problem::new(status.as_u16(), detail);
    let mut response = (status, Json(body)).into_response();
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static(Problem::CONTENT_TYPE),
    );
    response
}

impl IntoResponse for HostError {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::Missing(_) | Self::Name(_) => StatusCode::NOT_FOUND,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Stopped(_) | Self::Store(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        if status.is_server_error() {
            tracing::error!(error = %self, "a request failed");
        }
        problem(status, self.to_string())
    }
}

/// Requires the host's token on every request, when it has one.
async fn guard(State(host): State<Host>, request: Request, next: Next) -> Response {
    if let Some(token) = &host.config().token {
        let authorization = request.headers().get(AUTHORIZATION);
        let authorization = authorization.and_then(|value| value.to_str().ok());
        if !authorized(token, authorization) {
            return problem(
                StatusCode::UNAUTHORIZED,
                "this host requires its bearer token",
            );
        }
    }
    next.run(request).await
}

async fn pairs(State(host): State<Host>) -> Json<Vec<MindStatus>> {
    Json(host.pairs().await)
}

async fn pair(
    State(host): State<Host>,
    Path(name): Path<String>,
) -> Result<Json<MindStatus>, HostError> {
    Ok(Json(host.pair(&Name::new(name)?).await?))
}

async fn prompt(
    State(host): State<Host>,
    Path(name): Path<String>,
    Json(body): Json<PromptBody>,
) -> Result<Response, HostError> {
    let status = host.prompt(&Name::new(name)?, body.key, body.text).await?;
    let code = StatusCode::from_u16(status.status()).unwrap_or(StatusCode::OK);
    Ok((code, Json(PromptReply { status })).into_response())
}

async fn interrupt(
    State(host): State<Host>,
    Path(name): Path<String>,
) -> Result<Json<MindStatus>, HostError> {
    Ok(Json(host.interrupt(&Name::new(name)?).await?))
}

#[derive(Deserialize)]
struct Cursor {
    #[serde(default)]
    after: u64,
}

async fn history(
    State(host): State<Host>,
    Path(name): Path<String>,
    Query(cursor): Query<Cursor>,
) -> Result<Response, HostError> {
    let items = read(&host, &Name::new(name)?, cursor.after).await?;
    Ok(Json(History::from(cursor.after, &items)).into_response())
}

/// Reads a pair's history off the runtime's own threads, because a store
/// may wait on a disk.
async fn read(host: &Host, name: &Name, from: u64) -> Result<Vec<Item>, HostError> {
    let (host, name) = (host.clone(), name.clone());
    let read = tokio::task::spawn_blocking(move || host.history(&name, from)).await;
    read.unwrap_or_else(|error| Err(HostError::Store(StoreError::Backend(error.to_string()))))
}

/// A pair's history as server-sent events: every item from the cursor, then
/// each new one as it is stored. An event's id is its item's index, so a
/// client that reconnects resumes with `after` set to the last id plus one.
///
/// Between the items come `writing` events, which have no id: the text of
/// the reply the model is writing, as a [`mensor_api::Writing`]. They are
/// told in the order they happened, so the text of a reply always comes
/// before the step that holds it.
async fn events(
    State(host): State<Host>,
    Path(name): Path<String>,
    Query(cursor): Query<Cursor>,
) -> Result<Response, HostError> {
    let name = Name::new(name)?;
    let told = host.follow(&name).await?;
    // The history is read once at the start, and again whenever it grew.
    let state = (host, name, cursor.after, VecDeque::new(), told, true);
    let events = stream::unfold(
        state,
        |(host, name, mut next, mut ready, mut told, mut grew)| async move {
            loop {
                if let Some((index, item)) = ready.pop_front() {
                    let event = Event::default()
                        .id(u64::to_string(&index))
                        .json_data(&item)
                        .ok()?;
                    return Some((Ok(event), (host, name, next, ready, told, grew)));
                }
                if grew {
                    let page = read(&host, &name, next).await.ok()?;
                    // A full page may not be the last.
                    grew = page.len() == PAGE;
                    for item in page {
                        ready.push_back((next, item));
                        next += 1;
                    }
                    continue;
                }
                match told.recv().await {
                    Ok(Told::Wrote(writing)) => {
                        let event = Event::default().event(Writing::EVENT).data(&*writing);
                        return Some((Ok(event), (host, name, next, ready, told, grew)));
                    }
                    // One that fell behind has missed some of what was
                    // told, and reads the history to find what is new.
                    Ok(Told::Grew) | Err(RecvError::Lagged(_)) => grew = true,
                    Err(RecvError::Closed) => return None,
                }
            }
        },
    );
    Ok(sse(events).into_response())
}

fn sse(
    events: impl Stream<Item = Result<Event, Infallible>> + Send + 'static,
) -> Sse<impl Stream<Item = Result<Event, Infallible>> + Send + 'static> {
    Sse::new(events).keep_alive(KeepAlive::default())
}

async fn attach(State(host): State<Host>, upgrade: WebSocketUpgrade) -> Response {
    upgrade
        .max_message_size(MAX_FRAME_BYTES)
        .on_upgrade(move |socket| serve(host, socket))
}

/// Serves one hand's connection: reads its `hello`, joins it to a mind, then
/// carries messages both ways until either end is gone.
async fn serve(host: Host, mut socket: WebSocket) {
    let hello = match timeout(host.config().hello_timeout, hello(&mut socket)).await {
        Ok(Ok(hello)) => hello,
        Ok(Err(refusal)) => return close(socket, refusal.code(), refusal.reason()).await,
        Err(_) => {
            let refusal = Refusal::Protocol;
            return close(socket, refusal.code(), "no hello in time").await;
        }
    };
    let name = hello.name.clone();
    let mut connection = match host.connect(hello).await {
        Ok(connection) => connection,
        Err(Rejected::Refused(refusal)) => {
            return close(socket, refusal.code(), refusal.reason()).await;
        }
        Err(Rejected::Failed(_)) => return close(socket, 1011, "the host failed").await,
    };

    let idle = host.config().idle_timeout;
    let (mut sink, mut frames) = socket.split();
    loop {
        tokio::select! {
            out = connection.next() => match out {
                Some(Outgoing::Message(message)) => {
                    if sink.send(Message::Text(message.encode().into())).await.is_err() {
                        break;
                    }
                }
                Some(Outgoing::Close(refusal)) => {
                    let frame = CloseFrame { code: refusal.code(), reason: refusal.reason().into() };
                    let _ = sink.send(Message::Close(Some(frame))).await;
                    break;
                }
                None => break,
            },
            frame = timeout(idle, frames.next()) => match frame {
                Ok(Some(Ok(Message::Text(text)))) => match FromHand::decode(&text) {
                    Ok(message) => {
                        if !connection.send(message) {
                            break;
                        }
                    }
                    Err(error) => {
                        tracing::warn!(hand = %name, %error, "a hand sent what the protocol does not define");
                        let refusal = Refusal::Protocol;
                        let frame = CloseFrame { code: refusal.code(), reason: refusal.reason().into() };
                        let _ = sink.send(Message::Close(Some(frame))).await;
                        break;
                    }
                },
                Ok(Some(Ok(Message::Close(_)) | Err(_)) | None) | Err(_) => break,
                Ok(Some(Ok(_))) => {}
            },
        }
    }
}

/// Reads a connection's first message, which must be a `hello` of this
/// version.
async fn hello(socket: &mut WebSocket) -> Result<Hello, Refusal> {
    loop {
        match socket.recv().await {
            Some(Ok(Message::Text(text))) => {
                return match version_of(&text) {
                    Some(VERSION) => match FromHand::decode(&text) {
                        Ok(FromHand::Hello(hello)) => Ok(*hello),
                        _ => Err(Refusal::Protocol),
                    },
                    Some(_) => Err(Refusal::Version),
                    None => Err(Refusal::Protocol),
                };
            }
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            _ => return Err(Refusal::Protocol),
        }
    }
}

async fn close(mut socket: WebSocket, code: u16, reason: &'static str) {
    let frame = CloseFrame {
        code,
        reason: reason.into(),
    };
    let _ = socket.send(Message::Close(Some(frame))).await;
}

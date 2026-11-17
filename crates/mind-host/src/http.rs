use std::collections::VecDeque;
use std::convert::Infallible;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Extension, FromRequest, FromRequestParts, Path, Query, Request, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, HOST, ORIGIN};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream::{self, Stream};
use futures_util::{SinkExt, StreamExt};
use mensor_api::{
    Access, History, MakeBody, MindStatus, PAGE, Problem, PromptBody, PromptReply, ReleaseBody,
    Token, Writing, local,
};
use mensor_mind::{Item, StoreError};
use mensor_protocol::{FromHand, Hello, MAX_FRAME_BYTES, NOT_LOCAL, Name, Refusal};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::timeout;

use crate::host::{Host, HostError, Rejected};
use crate::task::{Outgoing, Told, stopped};

impl Host {
    /// The attach endpoint and the API as an [`axum`] router.
    pub fn router(&self) -> Router {
        Router::new()
            .route("/v1/attach", get(attach))
            .route("/v1/minds", get(minds))
            .route("/v1/minds/{name}", get(mind).put(make))
            .route("/v1/minds/{name}/prompt", post(prompt))
            .route("/v1/minds/{name}/interrupt", post(interrupt))
            .route("/v1/minds/{name}/release", post(release))
            .route("/v1/minds/{name}/end", post(end))
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
            Self::Missing(_) | Self::Name(_) | Self::NoHand(_) => StatusCode::NOT_FOUND,
            Self::NotRoot(_) | Self::Model { .. } | Self::Effort { .. } | Self::Unbranched(_) => {
                StatusCode::BAD_REQUEST
            }
            Self::Conflict(_) | Self::Exists(_) => StatusCode::CONFLICT,
            Self::Stopped(_) | Self::Store(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        if status.is_server_error() {
            tracing::error!(error = %self, "a request failed");
        }
        problem(status, self.to_string())
    }
}

/// Finds what a request reaches, and turns away one that reaches nothing.
///
/// With a token, a request carries it, or one made from it for one mind
/// ([`mensor_api::mind_token`]), as a bearer token. A host with no token trusts
/// whoever reaches it, so it serves only requests from its own machine
/// ([`mensor_api::local`]).
async fn guard(State(host): State<Host>, mut request: Request, next: Next) -> Response {
    match access(host.config().token.as_ref(), request.headers()) {
        Ok(access) => {
            request.extensions_mut().insert(access);
            next.run(request).await
        }
        Err((status, detail)) => problem(status, detail),
    }
}

/// What a request with these headers reaches on a host whose token is
/// `token`, or the status and the reason it is turned away with.
fn access(
    token: Option<&Token>,
    headers: &HeaderMap,
) -> Result<Access, (StatusCode, &'static str)> {
    let header = |name| headers.get(name).and_then(|value| value.to_str().ok());
    match token {
        Some(token) => Access::of(token, header(AUTHORIZATION)).ok_or((
            StatusCode::UNAUTHORIZED,
            "this host requires its bearer token",
        )),
        None if local(header(HOST), header(ORIGIN)) => Ok(Access::Host),
        None => Err((StatusCode::FORBIDDEN, NOT_LOCAL)),
    }
}

/// A request's JSON body. One that cannot be read as it, for a missing
/// content type, malformed JSON or a field it does not have, is refused
/// with a 400 problem, as every other refusal is a problem.
struct Body<T>(T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for Body<T> {
    type Rejection = Response;

    async fn from_request(request: Request, state: &S) -> Result<Self, Response> {
        match Json::<T>::from_request(request, state).await {
            Ok(Json(body)) => Ok(Self(body)),
            Err(rejection) => Err(problem(StatusCode::BAD_REQUEST, rejection.body_text())),
        }
    }
}

/// The mind a request's path names, which its token reaches.
struct Reached(Name);

impl<S: Send + Sync> FromRequestParts<S> for Reached {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Response> {
        let Path(name) = Path::<String>::from_request_parts(parts, state)
            .await
            .map_err(IntoResponse::into_response)?;
        let name = Name::new(name).map_err(|error| HostError::from(error).into_response())?;
        let access = parts.extensions.get::<Access>();
        if !access.is_some_and(|access| access.reaches(&name)) {
            let detail = format!("this token does not reach the mind {name}");
            return Err(problem(StatusCode::FORBIDDEN, detail));
        }
        Ok(Self(name))
    }
}

/// Every mind the request reaches. Each is listed without its system
/// prompt, which a fork's children share with it: `GET` one mind for that.
async fn minds(
    State(host): State<Host>,
    Extension(access): Extension<Access>,
) -> Json<Vec<MindStatus>> {
    let mut minds = host.minds().await;
    minds.retain(|status| access.reaches(&status.mind.name));
    for status in &mut minds {
        status.mind.settings.system = None;
    }
    Json(minds)
}

async fn mind(
    State(host): State<Host>,
    Reached(name): Reached,
) -> Result<Json<MindStatus>, HostError> {
    Ok(Json(host.mind(&name).await?))
}

/// Makes a mind. One made from another's history needs a token that
/// reaches that mind too.
async fn make(
    State(host): State<Host>,
    Extension(access): Extension<Access>,
    Reached(name): Reached,
    Body(body): Body<MakeBody>,
) -> Result<Response, HostError> {
    if let Some(from) = &body.from
        && !access.reaches(&from.mind)
    {
        let detail = format!("this token does not reach the mind {}", from.mind);
        return Ok(problem(StatusCode::FORBIDDEN, detail));
    }
    let settings = body.settings();
    let made = host.make(&name, settings, body.from).await?;
    let status = host.mind(&name).await?;
    let code = StatusCode::from_u16(made.status()).unwrap_or(StatusCode::OK);
    Ok((code, Json(status)).into_response())
}

async fn end(
    State(host): State<Host>,
    Reached(name): Reached,
) -> Result<Json<MindStatus>, HostError> {
    Ok(Json(host.end(&name).await?))
}

async fn prompt(
    State(host): State<Host>,
    Reached(name): Reached,
    Body(body): Body<PromptBody>,
) -> Result<Response, HostError> {
    let status = host.prompt(&name, body.key, body.text).await?;
    let code = StatusCode::from_u16(status.status()).unwrap_or(StatusCode::OK);
    Ok((code, Json(PromptReply { status })).into_response())
}

async fn interrupt(
    State(host): State<Host>,
    Reached(name): Reached,
) -> Result<Json<MindStatus>, HostError> {
    Ok(Json(host.interrupt(&name).await?))
}

async fn release(
    State(host): State<Host>,
    Reached(name): Reached,
    Body(body): Body<ReleaseBody>,
) -> Result<Json<MindStatus>, HostError> {
    Ok(Json(host.release(&name, body.hand).await?))
}

#[derive(Deserialize)]
struct Cursor {
    #[serde(default)]
    after: u64,
}

async fn history(
    State(host): State<Host>,
    Reached(name): Reached,
    Query(cursor): Query<Cursor>,
) -> Result<Response, HostError> {
    let items = read(&host, &name, cursor.after).await?;
    Ok(Json(History::from(cursor.after, &items)).into_response())
}

/// Reads a mind's history off the runtime's own threads, because a store
/// may wait on a disk.
async fn read(host: &Host, name: &Name, from: u64) -> Result<Vec<Item>, HostError> {
    let (host, name) = (host.clone(), name.clone());
    let read = tokio::task::spawn_blocking(move || host.history(&name, from)).await;
    read.unwrap_or_else(|error| Err(HostError::Store(StoreError::Backend(error.to_string()))))
}

/// A mind's history as server-sent events: every item from the cursor, then
/// each new one as it is stored. An event's id is its item's index, so a
/// client that reconnects resumes with `after` set to the last id plus one.
///
/// Between the items come `writing` events, which have no id: the text of
/// the reply the model is writing, as a [`mensor_api::Writing`]. They are
/// told in the order they happened, so the text of a reply always comes
/// before the step that holds it.
async fn events(
    State(host): State<Host>,
    Reached(name): Reached,
    Query(cursor): Query<Cursor>,
) -> Result<Response, HostError> {
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

async fn attach(
    State(host): State<Host>,
    Extension(access): Extension<Access>,
    upgrade: WebSocketUpgrade,
) -> Response {
    upgrade
        .max_message_size(MAX_FRAME_BYTES)
        .on_upgrade(move |socket| serve(host, socket, access))
}

/// Serves one hand's connection: reads its `hello`, joins it to the mind it
/// names if its token reaches that mind, then carries messages both ways
/// until either end is gone.
async fn serve(host: Host, mut socket: WebSocket, access: Access) {
    let hello = match timeout(host.config().hello_timeout, hello(&mut socket)).await {
        Ok(Ok(hello)) => hello,
        Ok(Err(refusal)) => return close(socket, refusal.code(), refusal.reason()).await,
        Err(_) => {
            let refusal = Refusal::Protocol;
            return close(socket, refusal.code(), "no hello in time").await;
        }
    };
    let (mind, hand) = (hello.mind.clone(), hello.hand.clone());
    if !access.reaches(&mind) {
        let refusal = Refusal::Forbidden;
        tracing::debug!(%mind, %hand, reason = refusal.reason(), "a hand was refused");
        return close(socket, refusal.code(), refusal.reason()).await;
    }
    let mut connection = match host.connect(hello).await {
        Ok(connection) => connection,
        Err(Rejected::Refused(refusal)) => {
            return close(socket, refusal.code(), refusal.reason()).await;
        }
        Err(Rejected::Failed(_)) => return close(socket, 1011, "the host failed").await,
    };

    let idle = host.config().idle_timeout;
    let mut stopping = host.stopping();
    let (mut sink, mut frames) = socket.split();
    loop {
        tokio::select! {
            () = stopped(&mut stopping) => {
                let frame = CloseFrame { code: 1001, reason: "the host stopped".into() };
                let _ = sink.send(Message::Close(Some(frame))).await;
                break;
            }
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
                        tracing::warn!(%mind, %hand, %error, "a hand sent what the protocol does not define");
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

/// Reads a connection's first message, which must be a `hello`.
async fn hello(socket: &mut WebSocket) -> Result<Hello, Refusal> {
    loop {
        match socket.recv().await {
            Some(Ok(Message::Text(text))) => {
                return match FromHand::decode(&text) {
                    Ok(FromHand::Hello(hello)) => Ok(*hello),
                    _ => Err(Refusal::Protocol),
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

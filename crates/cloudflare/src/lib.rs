//! mensor on Cloudflare Workers: each mind is one Durable Object.
//!
//! A `Mind` object holds a mind, its one hand and the hand's files in the
//! object's own `SQLite` database. Nothing dials in: the mind and the hand
//! pass the protocol's messages in memory, through
//! [`mensor_object::Object`]. The hand's capabilities are `files`, kept as
//! rows in that database, and `fetch`.
//!
//! The Worker in front of the objects serves the API `mensor serve` does,
//! for one mind at a time:
//!
//! - `POST /v1/minds/{name}/prompt` stores a prompt. The first prompt to a
//!   name starts its mind.
//! - `POST /v1/minds/{name}/interrupt` stops its turn.
//! - `GET /v1/minds/{name}` says what the mind is doing.
//! - `GET /v1/minds/{name}/history` and `/events` read its history, the
//!   second as server-sent events.
//!
//! An object does its work on its alarm, so a prompt is answered before the
//! model is asked, and an object the platform stops mid-work is woken to
//! carry on from what it stored.

mod api;
mod mind;
mod provider;
mod sql;
mod web;

use serde::Serialize;
use worker::{Context, Env, Request, Response, Result, event};

use mensor_api::Problem;

/// The binding of the mind objects, as `wrangler.toml` names it.
const MINDS: &str = "MINDS";

/// Sends each request to the object of the mind it names.
#[event(fetch)]
async fn fetch(request: Request, env: Env, _context: Context) -> Result<Response> {
    let url = request.url()?;
    let token = env
        .var("MENSOR_TOKEN")
        .ok()
        .map(|token| token.to_string())
        .filter(|token| !token.is_empty());
    let authorization = request.headers().get("authorization")?;
    let host = url.host_str().unwrap_or_default();
    if !api::permitted(token.as_deref(), authorization.as_deref(), host) {
        let detail = if token.is_some() {
            "this host requires its bearer token"
        } else {
            "this host has no MENSOR_TOKEN, so it serves only requests addressed to its own machine"
        };
        return problem(&Problem::new(401, detail));
    }
    match api::route(request.method().as_ref(), url.path()) {
        Ok(route) => {
            let object = env
                .durable_object(MINDS)?
                .get_by_name(route.name().as_str())?;
            object.fetch_with_request(request).await
        }
        Err(refused) => problem(&refused),
    }
}

/// A JSON answer.
fn json<T: Serialize>(status: u16, body: &T) -> Result<Response> {
    Ok(Response::from_json(body)?.with_status(status))
}

/// A refusal, as RFC 9457 problem details.
fn problem(refused: &Problem) -> Result<Response> {
    let mut response = json(refused.status, refused)?;
    response
        .headers_mut()
        .set("content-type", Problem::CONTENT_TYPE)?;
    Ok(response)
}

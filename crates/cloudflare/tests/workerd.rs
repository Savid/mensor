//! The worker on the real runtime: `wrangler dev` runs it on workerd, with a
//! scripted model beside it.
//!
//! These tests need Node, and the worker built by `make build-cloudflare`.
//! `make test-cloudflare` does both. They are ignored otherwise.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use reqwest::Client;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::time::sleep;

/// The wrangler that runs the worker. `WRANGLER` names another.
const WRANGLER: &str = "wrangler@4.160.1";

/// The token the worker under test requires.
const TOKEN: &str = "token-for-tests";

/// One thing the scripted model does with a request.
enum Scripted {
    /// Answer with this message.
    Answer(Value),
    /// Never answer: the request hangs until its sender is gone.
    Hold,
}

/// A stand-in for Claude's Messages API, and a slow page for `fetch`.
#[derive(Clone, Default)]
struct Model {
    script: Arc<Mutex<VecDeque<Scripted>>>,
    requests: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
    slow: Arc<AtomicUsize>,
}

impl Model {
    async fn serve() -> (Self, String) {
        let model = Self::default();
        let router = Router::new()
            .route("/v1/messages", post(messages))
            .route("/slow", get(slow))
            .with_state(model.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await });
        (model, format!("http://{address}"))
    }

    fn then(&self, content: Value) -> &Self {
        self.then_costing(content, 1)
    }

    /// Adds an answer that says it read `input` tokens.
    fn then_costing(&self, content: Value, input: u64) -> &Self {
        let uses_tools = content
            .as_array()
            .is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "tool_use"));
        let stop = if uses_tools { "tool_use" } else { "end_turn" };
        let mut answer = json!({
            "stop_reason": stop,
            "usage": {"input_tokens": input, "output_tokens": 1},
        });
        answer["content"] = content;
        self.push(Scripted::Answer(answer))
    }

    fn then_hold(&self) -> &Self {
        self.push(Scripted::Hold)
    }

    fn push(&self, scripted: Scripted) -> &Self {
        lock(&self.script).push_back(scripted);
        self
    }

    fn asked(&self) -> usize {
        lock(&self.requests).len()
    }

    fn request(&self, index: usize) -> (HeaderMap, Value) {
        lock(&self.requests)[index].clone()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

async fn messages(
    State(model): State<Model>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    lock(&model.requests).push((headers, body));
    let scripted = lock(&model.script).pop_front();
    match scripted {
        Some(Scripted::Answer(answer)) => {
            ([("content-type", "text/event-stream")], streamed(&answer)).into_response()
        }
        Some(Scripted::Hold) => std::future::pending().await,
        None => (
            StatusCode::BAD_REQUEST,
            Json(json!({"type": "error", "error": {
                "type": "invalid_request_error",
                "message": "the script has no reply left",
            }})),
        )
            .into_response(),
    }
}

/// A message as the events the API streams it in: a text in two pieces,
/// and a tool's input as JSON text.
fn streamed(answer: &Value) -> String {
    let mut events = vec![json!({"type": "message_start", "message": {
        "role": "assistant", "content": [], "usage": answer["usage"],
    }})];
    for (index, block) in answer["content"].as_array().unwrap().iter().enumerate() {
        let (start, deltas) = if let Some(text) = block["text"].as_str() {
            let middle = text.len() / 2;
            let pieces = [&text[..middle], &text[middle..]];
            let deltas = pieces.map(|piece| json!({"type": "text_delta", "text": piece}));
            (json!({"type": "text", "text": ""}), deltas.to_vec())
        } else {
            let mut start = block.clone();
            start["input"] = json!({});
            let input = block["input"].to_string();
            (
                start,
                vec![json!({"type": "input_json_delta", "partial_json": input})],
            )
        };
        events.push(json!({"type": "content_block_start", "index": index, "content_block": start}));
        for delta in deltas {
            events.push(json!({"type": "content_block_delta", "index": index, "delta": delta}));
        }
        events.push(json!({"type": "content_block_stop", "index": index}));
    }
    events.push(json!({"type": "message_delta",
                       "delta": {"stop_reason": answer["stop_reason"]},
                       "usage": {"output_tokens": answer["usage"]["output_tokens"]}}));
    events.push(json!({"type": "message_stop"}));
    events.iter().fold(String::new(), |mut stream, event| {
        let _ = write!(
            stream,
            "event: {}\ndata: {event}\n\n",
            event["type"].as_str().unwrap()
        );
        stream
    })
}

/// A page that is asked for and never arrives.
async fn slow(State(model): State<Model>) -> &'static str {
    model.slow.fetch_add(1, Ordering::SeqCst);
    std::future::pending().await
}

fn text(text: &str) -> Value {
    json!([{"type": "text", "text": text}])
}

fn uses(id: &str, name: &str, input: &Value) -> Value {
    json!([{"type": "tool_use", "id": id, "name": name, "input": input}])
}

/// The worker, run by `wrangler dev` over a directory that outlives it.
struct Worker {
    process: Child,
    url: String,
    http: Client,
}

impl Worker {
    /// Starts the worker over `directory`, where its objects' databases are
    /// kept, with its model at `model`.
    ///
    /// The ports it is given were free a moment before, and may not be by
    /// the time it listens: every test here picks ports at once. A worker
    /// that does not come up is said to have failed, with what wrangler
    /// wrote, and another is started on other ports.
    async fn start(directory: &Path, model: &str) -> Self {
        let deadline = Instant::now() + Duration::from_mins(3);
        loop {
            let mut worker = Self::spawn(directory, model).await;
            let Err(answered) = worker.up(Duration::from_secs(20)).await else {
                return worker;
            };
            let exited = worker.process.try_wait().ok().flatten();
            let written = std::fs::read_to_string(directory.join("wrangler.log"));
            let written = written.unwrap_or_default();
            let from = written.len().saturating_sub(1500);
            let said = written.get(from..).unwrap_or(&written);
            eprintln!(
                "a worker at {} did not start (exited: {exited:?}), and last answered: \
                 {answered}\nwrangler wrote:\n{said}",
                worker.url
            );
            worker.kill();
            assert!(Instant::now() < deadline, "the worker did not start");
        }
    }

    async fn spawn(directory: &Path, model: &str) -> Self {
        let port = free_port().await;
        let inspector = free_port().await;
        let wrangler = std::env::var("WRANGLER").unwrap_or_else(|_| WRANGLER.into());
        let log = std::fs::File::create(directory.join("wrangler.log")).unwrap();
        let mut command = Command::new("npx");
        command
            .args(["--yes", &wrangler, "dev", "--config"])
            .arg(config(directory))
            .arg("--persist-to")
            .arg(directory.join("state"))
            .args(["--ip", "127.0.0.1", "--port", &port.to_string()])
            .args(["--inspector-port", &inspector.to_string()])
            .args(["--var", "ANTHROPIC_API_KEY:key-for-tests"])
            .args(["--var", &format!("ANTHROPIC_BASE_URL:{model}")])
            .args(["--var", &format!("MENSOR_TOKEN:{TOKEN}")])
            .args(["--var", "MENSOR_SYSTEM:Be brief."])
            .args(["--var", "MENSOR_FALLBACKS:off"])
            .args(["--var", "MENSOR_COMPACT_AT:1000"])
            .args(["--var", "MENSOR_RECOVERY_MS:500"])
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            // The worker sees none of this machine's environment.
            .env_clear()
            .env("WRANGLER_SEND_METRICS", "false")
            .env("CI", "true");
        for kept in ["PATH", "HOME", "VOLTA_HOME"] {
            if let Ok(value) = std::env::var(kept) {
                command.env(kept, value);
            }
        }
        // Its own process group, so that the runtime it starts dies with it.
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let process = command.spawn().expect("npx runs: these tests need Node");
        Self {
            process,
            url: format!("http://127.0.0.1:{port}"),
            http: {
                // A client built with TLS in it needs its cryptography chosen.
                let _ = rustls::crypto::ring::default_provider().install_default();
                Client::new()
            },
        }
    }

    /// Waits until the worker is up, or says what it last answered.
    ///
    /// Wrangler's port answers before the worker behind it does, and a port
    /// may be answered by something else altogether. The worker is up once
    /// it refuses a request in its own words.
    async fn up(&mut self, within: Duration) -> Result<(), String> {
        let deadline = Instant::now() + within;
        loop {
            let answered = match self.http.get(self.pair("a1", "")).send().await {
                Ok(response) => {
                    let kind = response.headers().get("content-type").cloned();
                    let ours =
                        |kind: &reqwest::header::HeaderValue| kind == "application/problem+json";
                    if kind.as_ref().is_some_and(ours) {
                        return Ok(());
                    }
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    let body: String = body.chars().take(300).collect();
                    format!("{status} {kind:?}: {body}")
                }
                Err(error) => error.to_string(),
            };
            let exited = matches!(self.process.try_wait(), Ok(Some(_)));
            if exited || Instant::now() > deadline {
                return Err(answered);
            }
            sleep(Duration::from_millis(100)).await;
        }
    }

    /// Stops the runtime at once, as a crash or a deploy does: nothing it
    /// was doing gets to finish.
    fn kill(&mut self) {
        let group = format!("-{}", self.process.id());
        let mut kill = Command::new("kill");
        kill.args(["-KILL", "--", &group]).stderr(Stdio::null());
        let _ = kill.status();
        let _ = self.process.wait();
    }

    fn pair(&self, pair: &str, rest: &str) -> String {
        format!("{}/v1/pairs/{pair}{rest}", self.url)
    }

    async fn prompt(&self, pair: &str, body: Value) -> (StatusCode, Value) {
        let sent = self
            .http
            .post(self.pair(pair, "/prompt"))
            .bearer_auth(TOKEN)
            .json(&body);
        let response = sent.send().await.unwrap();
        let (status, answer) = (response.status(), response.text().await.unwrap());
        let read = serde_json::from_str(&answer);
        (
            status,
            read.unwrap_or_else(|_| panic!("{status}: {answer}")),
        )
    }

    async fn interrupt(&self, pair: &str) -> (StatusCode, Value) {
        let sent = self
            .http
            .post(self.pair(pair, "/interrupt"))
            .bearer_auth(TOKEN);
        let response = sent.send().await.unwrap();
        (response.status(), response.json().await.unwrap())
    }

    async fn get(&self, pair: &str, rest: &str) -> (StatusCode, Value) {
        let sent = self.http.get(self.pair(pair, rest)).bearer_auth(TOKEN);
        let response = sent.send().await.unwrap();
        (response.status(), response.json().await.unwrap())
    }

    async fn idle(&self, pair: &str) -> bool {
        let (_, status) = self.get(pair, "").await;
        status["state"] == "idle"
    }

    async fn history(&self, pair: &str) -> Vec<Value> {
        let (_, history) = self.get(pair, "/history").await;
        history["items"].as_array().unwrap().clone()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.kill();
    }
}

/// The worker's configuration without its build step, which `make
/// build-cloudflare` has already run, so that tests may start it side by
/// side.
fn config(directory: &Path) -> PathBuf {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let built = here.join("build/index.js");
    assert!(
        built.exists(),
        "the worker is not built: run `make build-cloudflare`"
    );
    let written = std::fs::read_to_string(here.join("wrangler.toml")).unwrap();
    let mut config = String::new();
    let mut in_build = false;
    for line in written.lines() {
        if line.starts_with('[') {
            in_build = line == "[build]";
        }
        if in_build {
            continue;
        }
        if line.starts_with("main = ") {
            let _ = writeln!(config, "main = {:?}", built.display().to_string());
        } else {
            config.push_str(line);
            config.push('\n');
        }
    }
    let path = directory.join("wrangler.toml");
    std::fs::write(&path, config).unwrap();
    path
}

async fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_mins(1);
    while !check().await {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        sleep(Duration::from_millis(50)).await;
    }
}

fn kinds(items: &[Value]) -> Vec<&str> {
    items
        .iter()
        .map(|item| item["type"].as_str().unwrap())
        .collect()
}

fn outcome<'a>(items: &'a [Value], tool_use: &str) -> &'a Value {
    let found = items
        .iter()
        .find(|item| item["type"] == "outcome" && item["toolUse"] == tool_use);
    &found.unwrap_or_else(|| panic!("{tool_use} has no outcome"))["outcome"]
}

/// The tool results of a request to the model, each as its text and
/// whether it is an error.
fn told(request: &Value) -> Vec<(String, bool)> {
    let messages = request["messages"].as_array().unwrap();
    messages
        .iter()
        .flat_map(|message| message["content"].as_array().unwrap())
        .filter(|block| block["type"] == "tool_result")
        .map(|block| {
            let text = block["content"].as_str().unwrap().to_owned();
            (text, block["is_error"] == true)
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Node and the built worker: run `make test-cloudflare`"]
async fn a_pair_runs_whole_turns_inside_one_object() {
    let directory = tempfile::tempdir().unwrap();
    let (model, model_url) = Model::serve().await;
    let worker = Worker::start(directory.path(), &model_url).await;

    // A prompt starts the pair. The model writes a file through the body.
    let haiku = json!({"path": "notes/haiku.txt", "content": "an old pond\n"});
    model
        .then(uses("tu_1", "write", &haiku))
        .then(text("Written."));
    worker
        .prompt("a1", json!({"text": "Write a haiku to notes/haiku.txt."}))
        .await;
    eventually("the first turn ends", || async {
        worker.idle("a1").await && worker.history("a1").await.len() == 6
    })
    .await;
    let items = worker.history("a1").await;
    assert_eq!(
        kinds(&items),
        ["attached", "prompt", "step", "call", "outcome", "step"]
    );
    assert_eq!(
        outcome(&items, "tu_1"),
        &json!({"output": "wrote 12 bytes to notes/haiku.txt"})
    );

    // The model was given the credential, the system prompt and the body's
    // operations as tools, and was told how its call ended.
    let (headers, first) = model.request(0);
    assert_eq!(headers["x-api-key"], "key-for-tests");
    assert_eq!(first["system"], "Be brief.");
    let tools: Vec<&str> = first["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(tools, ["read", "write", "edit", "list", "fetch"]);
    assert_eq!(
        told(&model.request(1).1),
        [("wrote 12 bytes to notes/haiku.txt".to_owned(), false)]
    );

    // The file is in the object's database: a later turn reads it back, and
    // finds it in its directory.
    model
        .then(json!([
            {"type": "tool_use", "id": "tu_2", "name": "read", "input": {"path": "/notes/haiku.txt"}},
            {"type": "tool_use", "id": "tu_3", "name": "list", "input": {"path": "notes"}},
        ]))
        .then(text("It says: an old pond."));
    worker.prompt("a1", json!({"text": "Read it back."})).await;
    eventually("the second turn ends", || async {
        worker.idle("a1").await && worker.history("a1").await.len() == 13
    })
    .await;
    let items = worker.history("a1").await;
    assert_eq!(outcome(&items, "tu_2"), &json!({"output": "an old pond\n"}));
    assert_eq!(outcome(&items, "tu_3"), &json!({"output": "haiku.txt"}));

    // Another pair is another object, with a database of its own.
    model
        .then(uses("tu_4", "list", &json!({})))
        .then(text("Nothing here."));
    worker
        .prompt("b2", json!({"text": "What files are there?"}))
        .await;
    eventually("the other pair's turn ends", || async {
        worker.idle("b2").await && worker.history("b2").await.len() == 6
    })
    .await;
    assert_eq!(
        outcome(&worker.history("b2").await, "tu_4"),
        &json!({"output": "the directory is empty"})
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Node and the built worker: run `make test-cloudflare`"]
async fn the_api_is_the_one_a_server_has() {
    let directory = tempfile::tempdir().unwrap();
    let (model, model_url) = Model::serve().await;
    let worker = Worker::start(directory.path(), &model_url).await;

    // Nothing is served without the token, and no pair exists unprompted.
    let unauthorized = worker.http.get(worker.pair("a1", "")).send().await.unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(worker.get("a1", "").await.0, StatusCode::NOT_FOUND);

    // A prompt is accepted before it is answered.
    model.then(text("Hello."));
    let prompt = json!({"key": "p1", "text": "Say hello."});
    let (status, accepted) = worker.prompt("a1", prompt.clone()).await;
    assert_eq!(
        (status, &accepted["status"]),
        (StatusCode::ACCEPTED, &json!("accepted"))
    );
    eventually("the turn ends", || async { worker.idle("a1").await }).await;
    let (_, status) = worker.get("a1", "").await;
    assert_eq!(
        status,
        json!({"name": "a1", "origin": "root", "state": "idle", "attached": true, "length": 3})
    );

    // The same prompt under its key is not a second prompt.
    let (status, existing) = worker.prompt("a1", prompt).await;
    assert_eq!(
        (status, &existing["status"]),
        (StatusCode::OK, &json!("existing"))
    );
    let (status, _) = worker
        .prompt("a1", json!({"key": "p1", "text": "Something else."}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(model.asked(), 1);

    // A history is read from any cursor, at once or as events.
    let (_, history) = worker.get("a1", "/history?after=2").await;
    assert_eq!(kinds(history["items"].as_array().unwrap()), ["step"]);
    assert_eq!(history["next"], 3);
    let events = worker.http.get(worker.pair("a1", "/events?after=1"));
    let mut events = events.bearer_auth(TOKEN).send().await.unwrap();
    assert_eq!(events.headers()["content-type"], "text/event-stream");
    let mut read = String::new();
    while !read.contains("id: 2\n") {
        let chunk = events.chunk().await.unwrap();
        read.push_str(&String::from_utf8_lossy(
            &chunk.expect("the stream stays open"),
        ));
    }
    assert!(
        read.starts_with("id: 1\ndata: {\"type\":\"prompt\""),
        "{read}"
    );

    // A reply is told as the model writes it, before the step that holds
    // it is stored.
    model.then(text("Hello again."));
    worker.prompt("a1", json!({"text": "And again."})).await;
    while !read.contains("id: 4\n") {
        let chunk = events.chunk().await.unwrap();
        read.push_str(&String::from_utf8_lossy(
            &chunk.expect("the stream stays open"),
        ));
    }
    let (before, _step) = read.split_once("id: 4\n").unwrap();
    let (_prompt, between) = before.split_once("id: 3\n").expect(&read);
    let written: String = between
        .split("event: writing\ndata: ")
        .skip(1)
        .map(|event| {
            let data = event.lines().next().unwrap();
            let writing: Value = serde_json::from_str(data).unwrap();
            writing["text"].as_str().unwrap().to_owned()
        })
        .collect();
    assert_eq!(written, "Hello again.", "{read}");
    assert!(between.contains("data: {\"at\":0,"), "{read}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Node and the built worker: run `make test-cloudflare`"]
async fn an_object_killed_while_its_model_is_asked_carries_on_by_itself() {
    let directory = tempfile::tempdir().unwrap();
    let (model, model_url) = Model::serve().await;
    let mut worker = Worker::start(directory.path(), &model_url).await;

    model.then_hold().then(text("Hello."));
    worker.prompt("a1", json!({"text": "Say hello."})).await;
    eventually("the model is asked", || async { model.asked() == 1 }).await;

    // The runtime dies with the model's answer still to come.
    worker.kill();
    let worker = Worker::start(directory.path(), &model_url).await;

    // Nobody asks the pair anything. Its alarm wakes it, and it asks again.
    eventually("the model is asked again", || async { model.asked() == 2 }).await;
    eventually("the turn ends", || async { worker.idle("a1").await }).await;
    let items = worker.history("a1").await;
    assert_eq!(kinds(&items), ["attached", "prompt", "step"]);
    assert_eq!(items[2]["content"], text("Hello."));
    assert_eq!(model.asked(), 2);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Node and the built worker: run `make test-cloudflare`"]
async fn a_call_in_flight_when_its_object_is_killed_is_lost_and_never_run_again() {
    let directory = tempfile::tempdir().unwrap();
    let (model, model_url) = Model::serve().await;
    let mut worker = Worker::start(directory.path(), &model_url).await;

    let page = json!({"url": format!("{model_url}/slow")});
    model
        .then(uses("tu_1", "fetch", &page))
        .then(text("I will check before I try again."));
    worker
        .prompt("a1", json!({"text": "Fetch the page."}))
        .await;
    eventually("the page is asked for", || async {
        model.slow.load(Ordering::SeqCst) == 1
    })
    .await;

    // The runtime dies while the body's call is still running.
    worker.kill();
    let worker = Worker::start(directory.path(), &model_url).await;

    // The call may have run, so it is not made again. The model is told.
    eventually("the model is asked what to do", || async {
        model.asked() == 2
    })
    .await;
    eventually("the turn ends", || async { worker.idle("a1").await }).await;
    let results = told(&model.request(1).1);
    assert!(
        matches!(&results[..], [(text, true)] if text.contains("its outcome is unknown")),
        "{results:?}"
    );
    assert_eq!(model.slow.load(Ordering::SeqCst), 1);
    let items = worker.history("a1").await;
    assert_eq!(
        kinds(&items),
        ["attached", "prompt", "step", "call", "outcome", "step"]
    );
    assert!(outcome(&items, "tu_1")["error"].is_string());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Node and the built worker: run `make test-cloudflare`"]
async fn an_interrupt_ends_the_turn_and_frees_the_object_at_once() {
    let directory = tempfile::tempdir().unwrap();
    let (model, model_url) = Model::serve().await;
    let worker = Worker::start(directory.path(), &model_url).await;

    // While the model is asked, and never answers.
    model.then_hold().then(text("A short one."));
    worker
        .prompt("a1", json!({"text": "Write a long essay."}))
        .await;
    eventually("the model is asked", || async { model.asked() == 1 }).await;
    let (status, pair) = worker.interrupt("a1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(pair["state"], "idle");
    let items = worker.history("a1").await;
    assert_eq!(kinds(&items), ["attached", "prompt", "paused"]);

    // The object does not wait for the answer it gave up on.
    worker
        .prompt("a1", json!({"text": "A short one, then."}))
        .await;
    eventually("the next turn ends", || async {
        model.asked() == 2 && worker.idle("a1").await
    })
    .await;
    let items = worker.history("a1").await;
    assert_eq!(
        kinds(&items),
        ["attached", "prompt", "paused", "prompt", "step"]
    );

    // While the body fetches a page that never arrives.
    let page = json!({"url": format!("{model_url}/slow")});
    model.then(uses("tu_1", "fetch", &page));
    worker
        .prompt("a1", json!({"text": "Fetch the page."}))
        .await;
    eventually("the page is asked for", || async {
        model.slow.load(Ordering::SeqCst) == 1
    })
    .await;
    let (_, pair) = worker.interrupt("a1").await;
    assert_eq!(pair["state"], "idle");
    let items = worker.history("a1").await;
    assert_eq!(outcome(&items, "tu_1")["error"], "cancelled");
    // The model is not asked what to do about a call the user stopped.
    assert_eq!(model.asked(), 3);

    // An interrupt with nothing to stop changes nothing.
    let stored = items.len();
    let (status, _) = worker.interrupt("a1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(worker.history("a1").await.len(), stored);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Node and the built worker: run `make test-cloudflare`"]
async fn an_object_wakes_from_its_last_summary_and_keeps_its_whole_history() {
    let directory = tempfile::tempdir().unwrap();
    let (model, model_url) = Model::serve().await;
    let mut worker = Worker::start(directory.path(), &model_url).await;

    // A step that read as much as a request may hold is summed up before
    // the next one.
    model.then_costing(text("Hi."), 999);
    let hello = json!({"key": "k1", "text": "Hello."});
    worker.prompt("a1", hello.clone()).await;
    eventually("the turn ends", || async { worker.idle("a1").await }).await;
    model
        .then(text("We greeted each other."))
        .then(text("Paris."));
    worker
        .prompt("a1", json!({"text": "What is the capital of France?"}))
        .await;
    eventually("the next turn ends", || async {
        worker.idle("a1").await && worker.history("a1").await.len() == 6
    })
    .await;
    let items = worker.history("a1").await;
    assert_eq!(
        kinds(&items),
        ["attached", "prompt", "step", "prompt", "summary", "step"]
    );
    // A mind starts from what the step before the summary had not seen.
    assert_eq!(items[4]["from"], 2);

    // The runtime dies. The object that wakes holds the end of its history,
    // from the summary, and its database the whole.
    worker.kill();
    let worker = Worker::start(directory.path(), &model_url).await;
    let (_, status) = worker.get("a1", "").await;
    assert_eq!(
        (&status["state"], &status["length"]),
        (&json!("idle"), &json!(6))
    );
    assert_eq!(worker.history("a1").await, items);
    let (_, page) = worker.get("a1", "/history?after=4").await;
    assert_eq!(
        kinds(page["items"].as_array().unwrap()),
        ["summary", "step"]
    );

    // A key from before the summary is still the prompt it was.
    let (status, existing) = worker.prompt("a1", hello).await;
    assert_eq!(
        (status, &existing["status"]),
        (StatusCode::OK, &json!("existing"))
    );
    let (status, _) = worker
        .prompt("a1", json!({"key": "k1", "text": "Goodbye."}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // The next step reads the summary and what followed it.
    model.then(text("Lyon is not."));
    worker.prompt("a1", json!({"text": "And Lyon?"})).await;
    eventually("the last turn ends", || async {
        worker.idle("a1").await && worker.history("a1").await.len() == 8
    })
    .await;
    let (_, asked) = model.request(model.asked() - 1);
    let messages = asked["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    assert!(
        messages[0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("We greeted each other.")
    );
}

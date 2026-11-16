//! A mind's host and real hands, joined over a real WebSocket on loopback.
//! The model is scripted. Commands run in temporary directories.

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use mensor::local::Local;
use mensor_hand::{Capabilities, Core, Limits};
use mensor_hand_host::capabilities::{Commands, Disk, Files};
use mensor_hand_host::{Backoff, Dial, Error, Handle, WebSocket, boot, start};
use mensor_mind::{
    Block, Content, Item, MemoryStore, ModelReply, Origin, Outcome, Role, Settings, State, Stop,
    Store, Usage,
};
use mensor_mind_host::{Config, Host, HostError, Prompted, Scripted};
use mensor_protocol::{Name, Refusal};
use mensor_store_sqlite::SqliteStore;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, timeout};

struct Server {
    host: Host,
    model: Arc<Scripted>,
    address: std::net::SocketAddr,
    task: JoinHandle<()>,
}

impl Server {
    async fn start(config: Config) -> Self {
        Self::over(Arc::new(MemoryStore::new()), config, "127.0.0.1:0").await
    }

    async fn over(store: Arc<dyn Store>, config: Config, address: &str) -> Self {
        let model = Arc::new(Scripted::new());
        let host = Host::open(store, model.clone(), config).unwrap();
        let listener = TcpListener::bind(address).await.unwrap();
        let address = listener.local_addr().unwrap();
        let serving = host.clone();
        let task = tokio::spawn(async move {
            serving
                .serve(listener, std::future::pending())
                .await
                .unwrap();
        });
        Self {
            host,
            model,
            address,
            task,
        }
    }

    fn attach(&self) -> String {
        format!("ws://{}/v1/attach", self.address)
    }

    fn api(&self, path: &str) -> String {
        format!("http://{}/v1{path}", self.address)
    }

    /// Starts a hand with `core`, whose commands run in `dir`.
    fn hand(&self, core: Core, dir: &Path) -> (Handle, JoinHandle<Result<(), Error>>) {
        self.hand_with(core, dir, None)
    }

    fn hand_with(
        &self,
        core: Core,
        dir: &Path,
        token: Option<&str>,
    ) -> (Handle, JoinHandle<Result<(), Error>>) {
        let dial = WebSocket::new(self.attach()).token(token.map(str::to_owned));
        hand_through(dial, core, dir)
    }

    /// Starts a hand in the host's own process, with no socket between them.
    fn local_hand(&self, core: Core, dir: &Path) -> (Handle, JoinHandle<Result<(), Error>>) {
        hand_through(Local::new(self.host.clone()), core, dir)
    }

    async fn prompt(&self, pair: &str, text: &str) {
        let prompted = self
            .host
            .prompt(&name(pair), None, text.into())
            .await
            .unwrap();
        assert_eq!(prompted, Prompted::Accepted);
    }

    /// Waits until the pair's hand is attached and its mind is idle.
    async fn settled(&self, pair: &str) {
        eventually(&format!("{pair} is attached and idle"), || async {
            self.host
                .pair(&name(pair))
                .await
                .is_ok_and(|status| status.attached && status.state == State::Idle)
        })
        .await;
    }

    fn history(&self, pair: &str) -> Vec<Item> {
        self.host.history(&name(pair), 0).unwrap()
    }

    /// Stops listening, and waits until the address is free again.
    async fn stop(mut self) {
        self.task.abort();
        let _ = (&mut self.task).await;
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn hand_through(
    dial: impl Dial + 'static,
    core: Core,
    dir: &Path,
) -> (Handle, JoinHandle<Result<(), Error>>) {
    let capabilities = Capabilities::new()
        .with(Commands::new(dir))
        .with(Files::new(Disk::new(dir)));
    let mut config = mensor_hand_host::Config::new(dial, capabilities);
    config.backoff = Backoff {
        min: Duration::from_millis(10),
        max: Duration::from_millis(50),
    };
    start(core, config)
}

async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(10);
    while !check().await {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        sleep(Duration::from_millis(10)).await;
    }
}

/// An HTTP client. One built with TLS in it needs its cryptography chosen,
/// as a host chooses it when it makes its model's client.
fn client() -> reqwest::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::new()
}

fn name(s: &str) -> Name {
    Name::new(s).unwrap()
}

fn fresh(hand: &str) -> Core {
    Core::new(name(hand), boot(), Limits::default())
}

fn text(text: &str) -> Block {
    Block::Text { text: text.into() }
}

fn bash(id: &str, command: &str) -> Block {
    Block::ToolUse {
        id: id.into(),
        name: "bash".into(),
        input: json!({"command": command}),
    }
}

fn outcome<'a>(items: &'a [Item], tool_use: &str) -> &'a Outcome {
    items
        .iter()
        .find_map(|item| match item {
            Item::Outcome {
                tool_use: of,
                outcome,
                ..
            } if of == tool_use => Some(outcome),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no outcome for {tool_use}"))
}

fn stdout(outcome: &Outcome) -> &str {
    match outcome {
        Outcome::Output(output) => output["stdout"].as_str().unwrap(),
        other => panic!("not an output: {other:?}"),
    }
}

fn said(items: &[Item]) -> Vec<&str> {
    items
        .iter()
        .filter_map(|item| match item {
            Item::Step {
                content,
                stop: Stop::EndTurn,
                ..
            } => Some(content),
            _ => None,
        })
        .flatten()
        .filter_map(|block| match block {
            Block::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_prompt_runs_a_command_on_the_hand_and_is_answered() {
    let server = Server::start(Config::default()).await;
    server
        .model
        .then(vec![bash(
            "tu_1",
            "echo hello > greeting.txt && cat greeting.txt",
        )])
        .then(vec![text("It says hello.")]);
    let dir = tempfile::tempdir().unwrap();
    let (_hand, _task) = server.hand(fresh("a1"), dir.path());
    server.settled("a1").await;

    server
        .prompt("a1", "write a greeting and read it back")
        .await;
    server.settled("a1").await;

    assert_eq!(
        std::fs::read_to_string(dir.path().join("greeting.txt")).unwrap(),
        "hello\n"
    );
    let history = server.history("a1");
    assert_eq!(stdout(outcome(&history, "tu_1")), "hello\n");
    assert_eq!(said(&history), ["It says hello."]);

    // The model was offered the hand's capabilities as tools, and was told
    // what the command printed.
    let requests = server.model.requests();
    let tools: Vec<_> = requests[0]
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert_eq!(tools, ["bash", "read", "write", "edit", "list"]);
    let told = &requests[1].messages[2];
    assert_eq!(told.role, Role::User);
    assert!(
        matches!(&told.content[0], Content::ToolResult { text, error: false, .. } if text.contains("hello"))
    );

    let pairs = server.host.pairs().await;
    assert_eq!(pairs.len(), 1);
    assert_eq!(pairs[0].pair.origin, Origin::Root);
}

#[tokio::test]
async fn a_clone_attaches_and_the_mind_forks_to_match() {
    let server = Server::start(Config::default()).await;
    server
        .model
        .then(vec![bash("tu_1", "echo original > note.txt")])
        .then(vec![text("Written.")]);
    let dir = tempfile::tempdir().unwrap();
    let (hand, _task) = server.hand(fresh("a1"), dir.path());
    server.settled("a1").await;
    server.prompt("a1", "write a note").await;
    server.settled("a1").await;
    let parent = server.history("a1");

    // Clone the hand: copy what it remembers and its files, and feed the
    // copy a new name.
    let mut core = hand.snapshot().await.unwrap();
    core.rename(name("b7"));
    let copy = tempfile::tempdir().unwrap();
    std::fs::copy(dir.path().join("note.txt"), copy.path().join("note.txt")).unwrap();
    let (_clone, _clone_task) = server.hand(core, copy.path());
    server.settled("b7").await;

    // The new mind shares the parent's whole history, and waits.
    let pairs = server.host.pairs().await;
    let child = pairs
        .iter()
        .find(|status| status.pair.name == name("b7"))
        .unwrap();
    let inherited = u64::try_from(parent.len()).unwrap();
    assert_eq!(
        child.pair.origin,
        Origin::Fork {
            parent: name("a1"),
            inherited
        }
    );
    let history = server.history("b7");
    assert_eq!(history[..parent.len()], parent[..]);
    assert_eq!(
        history[parent.len()..],
        [Item::Paused {
            reason: "forked from a1".into()
        }]
    );
    assert_eq!(
        server.model.requests().len(),
        2,
        "a new mind calls no model"
    );

    // Prompted, it carries on in its own hand, with the parent's past.
    server
        .model
        .then(vec![bash("tu_2", "echo clone >> note.txt && cat note.txt")])
        .then(vec![text("Two lines.")]);
    server.prompt("b7", "add a line and show the note").await;
    server.settled("b7").await;

    let history = server.history("b7");
    assert_eq!(stdout(outcome(&history, "tu_2")), "original\nclone\n");
    assert_eq!(said(&history), ["Written.", "Two lines."]);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("note.txt")).unwrap(),
        "original\n"
    );
    assert_eq!(
        server.history("a1"),
        parent,
        "the parent's history is its own"
    );

    let first = &server.model.requests()[2].messages[0];
    assert_eq!(first.content, vec![Content::Text("write a note".into())]);
}

#[tokio::test]
async fn a_hand_copied_while_it_ran_a_command_tells_its_new_mind_the_command_was_lost() {
    let server = Server::start(Config::default()).await;
    server
        .model
        .then(vec![bash("tu_1", "sleep 0.4; echo finished")])
        .then(vec![text("Done.")]);
    let dir = tempfile::tempdir().unwrap();
    let (hand, _task) = server.hand(fresh("a1"), dir.path());
    server.settled("a1").await;
    server.prompt("a1", "wait a moment").await;
    eventually("the command is running", || async {
        hand.snapshot()
            .await
            .is_some_and(|core| core.running().count() == 1)
    })
    .await;

    // The copy remembers the call, but this process is not running it.
    let mut core = hand.snapshot().await.unwrap();
    core.rename(name("b7"));
    let (_clone, _clone_task) = server.hand(core, dir.path());
    server.settled("b7").await;
    server.settled("a1").await;

    assert_eq!(stdout(outcome(&server.history("a1"), "tu_1")), "finished\n");
    let lost = outcome(&server.history("b7"), "tu_1").clone();
    assert!(
        matches!(&lost, Outcome::Error(error) if error.contains("restarted while this call ran")),
        "{lost:?}"
    );
    assert_eq!(said(&server.history("b7")), Vec::<&str>::new());
}

#[tokio::test]
async fn a_lineage_takes_no_more_clones_than_its_limit() {
    let server = Server::start(Config {
        max_clones: 1,
        ..Config::default()
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let (hand, _task) = server.hand(fresh("a1"), dir.path());
    server.settled("a1").await;

    let clone = |clone: &'static str| async move {
        let mut core = hand.snapshot().await.unwrap();
        core.rename(name(clone));
        core
    };
    let (_first, _first_task) = server.hand(clone("b7").await, dir.path());
    server.settled("b7").await;

    // A clone of the clone is in the same lineage.
    let mut second = fresh("b7");
    second.rename(name("c9"));
    let (_second, refused) = server.hand(second, dir.path());
    let error = timeout(Duration::from_secs(10), refused)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(matches!(error, Error::Refused(Refusal::Limit)), "{error}");
    assert_eq!(server.host.pairs().await.len(), 2);
}

#[tokio::test]
async fn forking_is_off_when_the_limit_is_zero() {
    let server = Server::start(Config {
        max_clones: 0,
        ..Config::default()
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let (_hand, _task) = server.hand(fresh("a1"), dir.path());
    server.settled("a1").await;

    let mut clone = fresh("a1");
    clone.rename(name("b7"));
    let (_clone, refused) = server.hand(clone, dir.path());
    let error = timeout(Duration::from_secs(10), refused)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(matches!(error, Error::Refused(Refusal::Limit)), "{error}");
}

#[tokio::test]
async fn a_clone_dials_until_the_hand_it_was_cloned_from_is_known() {
    let server = Server::start(Config::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let mut clone = fresh("a1");
    clone.rename(name("b7"));
    let (_clone, clone_task) = server.hand(clone, dir.path());
    sleep(Duration::from_millis(150)).await;
    assert!(
        !clone_task.is_finished(),
        "an unknown parent is worth another dial"
    );
    assert!(server.host.pairs().await.is_empty());

    let (_hand, _task) = server.hand(fresh("a1"), dir.path());
    server.settled("b7").await;
}

#[tokio::test]
async fn a_dropped_connection_loses_no_result() {
    let server = Server::start(Config::default()).await;
    server
        .model
        .then(vec![bash("tu_1", "sleep 0.3; echo survived")])
        .then(vec![text("Done.")]);
    let dir = tempfile::tempdir().unwrap();
    let (hand, _task) = server.hand(fresh("a1"), dir.path());
    server.settled("a1").await;
    server.prompt("a1", "wait, then speak").await;
    eventually("the command is running", || async {
        hand.snapshot()
            .await
            .is_some_and(|core| core.running().count() == 1)
    })
    .await;

    // The command outlives the connection, and its result arrives on the
    // next one.
    hand.redial();
    server.settled("a1").await;

    let history = server.history("a1");
    assert_eq!(stdout(outcome(&history, "tu_1")), "survived\n");
    assert_eq!(
        history
            .iter()
            .filter(|item| matches!(item, Item::Call { .. }))
            .count(),
        1
    );
    assert_eq!(said(&history), ["Done."]);
}

#[tokio::test]
async fn a_host_that_restarts_carries_every_pair_on() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mensor.sqlite");
    let open = || -> Arc<dyn Store> { Arc::new(SqliteStore::open(&path).unwrap()) };

    let first = Server::over(open(), Config::default(), "127.0.0.1:0").await;
    first
        .model
        .then(vec![bash("tu_1", "echo one")])
        .then(vec![text("One.")]);
    let (hand, _task) = first.hand(fresh("a1"), dir.path());
    first.settled("a1").await;
    first.prompt("a1", "count").await;
    first.settled("a1").await;
    let address = first.address.to_string();
    let before = first.history("a1");
    first.stop().await;

    // A new host over the same file, on the same address. The hand drops
    // its old connection and finds the new host.
    let second = Server::over(open(), Config::default(), &address).await;
    assert_eq!(second.history("a1"), before);
    hand.redial();
    second.settled("a1").await;

    second
        .model
        .then(vec![bash("tu_2", "echo two")])
        .then(vec![text("Two.")]);
    second.prompt("a1", "count again").await;
    second.settled("a1").await;
    let history = second.history("a1");
    assert_eq!(history[..before.len()], before[..]);
    assert_eq!(stdout(outcome(&history, "tu_2")), "two\n");
    assert_eq!(said(&history), ["One.", "Two."]);
    // The hand kept its memory, so the mind recorded one boot and sent the
    // second call under the next id.
    assert_eq!(
        history
            .iter()
            .filter(|item| matches!(item, Item::Attached { .. }))
            .count(),
        1
    );
    assert!(
        history
            .iter()
            .any(|item| matches!(item, Item::Call { id: 2, .. }))
    );
}

#[tokio::test]
async fn the_api_prompts_a_pair_and_follows_its_history() {
    let server = Server::start(Config::default()).await;
    server.model.then(vec![text("Hello.")]);
    let dir = tempfile::tempdir().unwrap();
    let (_hand, _task) = server.hand(fresh("a1"), dir.path());
    server.settled("a1").await;
    let http = client();
    let prompt = |hand: Value| http.post(server.api("/minds/a1/prompt")).json(&hand).send();

    // A prompt under a key is accepted once, found again, and refused when
    // the key comes back with other words.
    let accepted = prompt(json!({"key": "k1", "text": "hello"})).await.unwrap();
    assert_eq!(accepted.status(), 202);
    assert_eq!(
        accepted.json::<Value>().await.unwrap(),
        json!({"status": "accepted"})
    );
    let again = prompt(json!({"key": "k1", "text": "hello"})).await.unwrap();
    assert_eq!(again.status(), 200);
    assert_eq!(
        again.json::<Value>().await.unwrap(),
        json!({"status": "existing"})
    );
    let conflict = prompt(json!({"key": "k1", "text": "goodbye"}))
        .await
        .unwrap();
    assert_eq!(conflict.status(), 409);
    assert_eq!(
        conflict.headers()["content-type"],
        "application/problem+json"
    );
    server.settled("a1").await;

    let pairs: Value = http
        .get(server.api("/minds"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        pairs,
        json!([{"name": "a1", "origin": "root", "state": "idle", "attached": true, "length": 3}])
    );

    let history: Value = http
        .get(server.api("/minds/a1/history?after=1"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(history["next"], json!(3));
    assert_eq!(
        history["items"][0],
        json!({"type": "prompt", "key": "k1", "text": "hello"})
    );
    assert_eq!(history["items"][1]["type"], json!("step"));

    // The event stream replays from a cursor: the id of each event is the
    // index of its item.
    let mut events = http
        .get(server.api("/minds/a1/events?after=1"))
        .send()
        .await
        .unwrap();
    assert_eq!(events.headers()["content-type"], "text/event-stream");
    let mut stream = String::new();
    while !stream.contains("id: 2\n") {
        let chunk = timeout(Duration::from_secs(5), events.chunk())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        stream.push_str(std::str::from_utf8(&chunk).unwrap());
    }
    assert!(
        stream.contains("id: 1\n") && stream.contains(r#""type":"prompt""#),
        "{stream}"
    );

    // A reply is told as the model writes it, before the step that holds
    // it is stored.
    server.model.then(vec![text("And again.")]);
    server.prompt("a1", "once more").await;
    while !stream.contains("id: 4\n") {
        let chunk = timeout(Duration::from_secs(5), events.chunk())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        stream.push_str(std::str::from_utf8(&chunk).unwrap());
    }
    let at = |event: &str| stream.find(event).unwrap_or_else(|| panic!("{stream}"));
    let writing = at("event: writing\ndata: {\"at\":0,\"text\":\"And again.\"}\n");
    assert!(
        at("id: 3\n") < writing && writing < at("id: 4\n"),
        "{stream}"
    );

    let missing = http.get(server.api("/minds/nobody")).send().await.unwrap();
    assert_eq!(missing.status(), 404);
    assert!(matches!(
        server.host.pair(&name("nobody")).await,
        Err(HostError::Missing(_))
    ));
}

#[tokio::test]
async fn a_host_with_a_token_serves_only_those_who_hold_it() {
    let config = Config {
        token: Some("secret".into()),
        ..Config::default()
    };
    let server = Server::start(config).await;
    let dir = tempfile::tempdir().unwrap();
    let http = client();

    let without = http.get(server.api("/minds")).send().await.unwrap();
    assert_eq!(without.status(), 401);
    let with = http
        .get(server.api("/minds"))
        .bearer_auth("secret")
        .send()
        .await
        .unwrap();
    assert_eq!(with.status(), 200);

    let (_stranger, refused) = server.hand_with(fresh("x1"), dir.path(), Some("wrong"));
    let error = timeout(Duration::from_secs(10), refused)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(matches!(error, Error::Unauthorized), "{error}");
    assert!(server.host.pairs().await.is_empty());

    let (_hand, _task) = server.hand_with(fresh("a1"), dir.path(), Some("secret"));
    server.settled("a1").await;
}

#[tokio::test]
async fn a_mind_and_a_hand_in_one_process_need_no_socket() {
    let server = Server::start(Config::default()).await;
    server
        .model
        .then(vec![bash("tu_1", "echo 1")])
        .then(vec![text("It printed 1.")]);
    let dir = tempfile::tempdir().unwrap();
    let (hand, _task) = server.local_hand(fresh("local"), dir.path());
    server.settled("local").await;

    server.prompt("local", "run echo 1").await;
    server.settled("local").await;
    let history = server.history("local");
    assert_eq!(stdout(outcome(&history, "tu_1")), "1\n");
    assert_eq!(said(&history), ["It printed 1."]);

    // A clone in the same process forks the mind, as one over a socket does.
    let mut core = hand.snapshot().await.unwrap();
    core.rename(name("twin"));
    let (_twin, _twin_task) = server.local_hand(core, dir.path());
    server.settled("twin").await;
    let inherited = u64::try_from(history.len()).unwrap();
    let pairs = server.host.pairs().await;
    let twin = pairs
        .iter()
        .find(|status| status.pair.name == name("twin"))
        .unwrap();
    assert_eq!(
        twin.pair.origin,
        Origin::Fork {
            parent: name("local"),
            inherited
        }
    );

    // A hand across a socket attaches to the same host beside them.
    let (_remote, _remote_task) = server.hand(fresh("remote"), dir.path());
    server.settled("remote").await;
    assert_eq!(server.host.pairs().await.len(), 3);
}

#[tokio::test]
async fn a_hand_in_the_same_process_is_refused_as_any_other() {
    let server = Server::start(Config {
        max_clones: 0,
        ..Config::default()
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let (_first, replaced) = server.local_hand(fresh("local"), dir.path());
    server.settled("local").await;

    let mut clone = fresh("local");
    clone.rename(name("twin"));
    let (_clone, refused) = server.local_hand(clone, dir.path());
    let error = timeout(Duration::from_secs(10), refused)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(matches!(error, Error::Refused(Refusal::Limit)), "{error}");

    // A second hand under a name in use takes the first one's place.
    let (_usurper, _usurper_task) = server.local_hand(fresh("local"), dir.path());
    let error = timeout(Duration::from_secs(10), replaced)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(
        matches!(error, Error::Refused(Refusal::Replaced)),
        "{error}"
    );
}

#[tokio::test]
async fn an_interrupt_kills_the_command_a_hand_runs_and_ends_the_turn() {
    let server = Server::start(Config::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let (_hand, _task) = server.hand(fresh("a1"), dir.path());
    server.settled("a1").await;

    server.model.then(vec![bash(
        "tu_1",
        "echo started > mark; sleep 60; echo finished > end",
    )]);
    server.prompt("a1", "Wait a minute.").await;
    eventually("the command has started", || async {
        dir.path().join("mark").exists()
    })
    .await;

    let interrupted = client()
        .post(server.api("/minds/a1/interrupt"))
        .send()
        .await
        .unwrap();
    assert_eq!(interrupted.status(), 200);
    server.settled("a1").await;

    let items = server.history("a1");
    assert_eq!(outcome(&items, "tu_1"), &Outcome::Error("cancelled".into()));
    assert!(items.contains(&Item::Paused {
        reason: "interrupted".into()
    }));
    assert!(!dir.path().join("end").exists());
    // The cancelled call was not handed to the model: the turn is over.
    assert_eq!(server.model.requests().len(), 1);

    server.model.then(vec![text("Stopped.")]);
    server.prompt("a1", "Never mind.").await;
    server.settled("a1").await;
    assert_eq!(said(&server.history("a1")), ["Stopped."]);
    let requests = server.model.requests();
    assert!(matches!(
        &requests[1].messages[2].content[0],
        Content::ToolResult { text, error: true, .. } if text == "cancelled"
    ));
}

#[tokio::test]
async fn a_conversation_past_its_limit_carries_on_from_a_summary() {
    let server = Server::start(Config {
        mind: Settings {
            compact_at: Some(100),
            ..Settings::default()
        },
        ..Config::default()
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let (_hand, _task) = server.hand(fresh("a1"), dir.path());
    server.settled("a1").await;

    server.model.then_with(|_| ModelReply {
        content: vec![text("hi")],
        stop: Stop::EndTurn,
        usage: Usage {
            input: 95,
            output: 5,
        },
    });
    server.prompt("a1", "hello").await;
    server.settled("a1").await;

    server
        .model
        .then(vec![text("We greeted each other.")])
        .then(vec![text("Paris.")]);
    server.prompt("a1", "What is the capital of France?").await;
    server.settled("a1").await;

    let items = server.history("a1");
    assert!(items.iter().any(|item| matches!(
        item,
        Item::Summary { text, .. } if text == "We greeted each other."
    )));
    assert_eq!(said(&items), ["hi", "Paris."]);
    let requests = server.model.requests();
    assert!(requests[1].text_only);
    // The step after the summary reads the summary and the prompt, and
    // nothing of what came before.
    assert_eq!(requests[2].messages.len(), 1);
    assert_eq!(requests[2].messages[0].content.len(), 2);
}

/// A host over a database file whose pairs rest soon, and sum up early.
async fn restful(path: &Path) -> Server {
    let config = Config {
        rest_after: Duration::from_millis(50),
        mind: Settings {
            compact_at: Some(100),
            ..Settings::default()
        },
        ..Config::default()
    };
    let store = Arc::new(SqliteStore::open(path).unwrap());
    Server::over(store, config, "127.0.0.1:0").await
}

/// Stops a hand, and waits until its pair has left the host's memory.
async fn rested(server: &Server, hand: &Handle) -> Core {
    let core = hand.snapshot().await.unwrap();
    hand.stop();
    eventually("the pair rests", || async {
        server.host.awake().await == 0
    })
    .await;
    core
}

#[tokio::test]
async fn a_pair_with_nothing_to_do_rests_in_the_store_and_wakes_when_needed() {
    let dir = tempfile::tempdir().unwrap();
    let server = restful(&dir.path().join("mensor.sqlite")).await;
    let a1 = name("a1");
    let (hand, _task) = server.hand(fresh("a1"), dir.path());
    server.settled("a1").await;
    server.model.then_with(|_| ModelReply {
        content: vec![text("hi")],
        stop: Stop::EndTurn,
        usage: Usage {
            input: 95,
            output: 5,
        },
    });
    let keyed = |text: &str| server.host.prompt(&a1, Some("k1".into()), text.into());
    assert_eq!(keyed("hello").await.unwrap(), Prompted::Accepted);
    server.settled("a1").await;

    // A pair whose hand is connected stays awake, however long it idles.
    sleep(Duration::from_millis(200)).await;
    assert_eq!(server.host.awake().await, 1);

    // One with no hand, no follower and no model asked leaves memory. What
    // it is doing is then read from the store, and wakes nothing.
    let core = rested(&server, &hand).await;
    let status = server.host.pair(&a1).await.unwrap();
    assert_eq!(
        (status.state, status.attached, status.length),
        (State::Idle, false, 3)
    );
    assert_eq!(server.host.pairs().await.len(), 1);
    assert_eq!(server.history("a1").len(), 3);
    assert_eq!(server.host.awake().await, 0);

    // A prompt wakes it for as long as it takes to store. With no hand the
    // mind asks nobody, and rests again mid-turn.
    server.prompt("a1", "What is the capital of France?").await;
    eventually("the pair rests again", || async {
        server.host.awake().await == 0
    })
    .await;
    let status = server.host.pair(&a1).await.unwrap();
    assert_eq!((status.state, status.length), (State::Thinking, 4));
    assert_eq!(server.model.requests().len(), 1);

    // Its hand wakes it, and the turn carries on: here by summing up.
    server
        .model
        .then(vec![text("We greeted each other.")])
        .then(vec![text("Paris.")]);
    let (hand, _task) = server.hand(core, dir.path());
    server.settled("a1").await;
    assert_eq!(said(&server.history("a1")), ["hi", "Paris."]);

    // Woken again, the mind is given only the end of its history: from
    // the summary. A key from before it is found in the store.
    let core = rested(&server, &hand).await;
    assert_eq!(keyed("hello").await.unwrap(), Prompted::Existing);
    assert!(matches!(
        keyed("goodbye").await,
        Err(HostError::Conflict(_))
    ));
    server.model.then(vec![text("Lyon is not.")]);
    let (_hand, _task) = server.hand(core, dir.path());
    server.settled("a1").await;
    server.prompt("a1", "And Lyon?").await;
    server.settled("a1").await;
    let requests = server.model.requests();
    let asked = &requests.last().unwrap().messages;
    assert_eq!(asked.len(), 3);
    assert!(matches!(
        &asked[0].content[0],
        Content::Text(summary) if summary.contains("We greeted each other.")
    ));
    assert_eq!(
        said(&server.history("a1")),
        ["hi", "Paris.", "Lyon is not."]
    );
}

#[tokio::test]
async fn a_pair_that_is_followed_stays_awake() {
    let dir = tempfile::tempdir().unwrap();
    let server = restful(&dir.path().join("mensor.sqlite")).await;
    let (hand, _task) = server.hand(fresh("a1"), dir.path());
    server.settled("a1").await;
    let events = client()
        .get(server.api("/minds/a1/events"))
        .send()
        .await
        .unwrap();
    hand.stop();
    sleep(Duration::from_millis(200)).await;
    assert_eq!(server.host.awake().await, 1);

    drop(events);
    // A stream that is dropped is noticed when the host next writes to it.
    server.prompt("a1", "anyone?").await;
    eventually("the pair rests", || async {
        server.host.awake().await == 0
    })
    .await;
}

#[tokio::test]
async fn a_clone_of_a_hand_whose_mind_summed_up_forks_from_the_whole_history() {
    let dir = tempfile::tempdir().unwrap();
    let server = restful(&dir.path().join("mensor.sqlite")).await;
    let (hand, _task) = server.hand(fresh("a1"), dir.path());
    server.settled("a1").await;
    server.model.then_with(|_| ModelReply {
        content: vec![bash("tu_1", "echo original > note.txt")],
        stop: Stop::ToolUse,
        usage: Usage {
            input: 95,
            output: 5,
        },
    });
    server
        .model
        .then(vec![text("We wrote a note.")])
        .then(vec![text("Written.")]);
    server.prompt("a1", "write a note").await;
    server.settled("a1").await;
    let parent = server.history("a1");
    assert!(
        parent
            .iter()
            .any(|item| matches!(item, Item::Summary { .. }))
    );

    // The parent rests, so its mind is nowhere but the store.
    let mut core = rested(&server, &hand).await;
    core.rename(name("b7"));
    let copy = tempfile::tempdir().unwrap();
    std::fs::copy(dir.path().join("note.txt"), copy.path().join("note.txt")).unwrap();
    let (_clone, _clone_task) = server.hand(core, copy.path());
    server.settled("b7").await;

    let child = server.history("b7");
    assert_eq!(child[..parent.len()], parent[..]);
    assert!(matches!(
        child.last(),
        Some(Item::Paused { reason }) if reason == "forked from a1"
    ));
    // Forking did not wake the parent.
    assert_eq!(server.host.awake().await, 1);

    server.model.then(vec![text("It says original.")]);
    server.prompt("b7", "what does the note say?").await;
    server.settled("b7").await;
    let requests = server.model.requests();
    let asked = &requests.last().unwrap().messages;
    assert!(matches!(
        &asked[0].content[0],
        Content::Text(summary) if summary.contains("We wrote a note.")
    ));
    assert_eq!(
        said(&server.history("b7")),
        ["Written.", "It says original."]
    );
}

#[tokio::test]
async fn a_long_history_is_read_a_page_at_a_time() {
    let store = Arc::new(MemoryStore::new());
    let pair = mensor_mind::Pair {
        name: name("a1"),
        origin: Origin::Root,
    };
    store.create(&pair).unwrap();
    let prompts: Vec<Item> = (0..1200)
        .map(|index| Item::Prompt {
            key: None,
            text: format!("prompt {index}"),
        })
        .collect();
    store.append(&name("a1"), &prompts).unwrap();
    let server = Server::over(store, Config::default(), "127.0.0.1:0").await;
    let http = client();

    // One answer holds a page, and says where the next begins.
    let mut next = 0;
    let mut read = 0;
    loop {
        let history: Value = http
            .get(server.api(&format!("/minds/a1/history?after={next}")))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let items = history["items"].as_array().unwrap().len();
        assert!(items <= 512);
        if items == 0 {
            break;
        }
        read += items;
        next = history["next"].as_u64().unwrap();
    }
    assert_eq!((read, next), (1200, 1200));

    // The events of a history carry on through every page of it.
    let mut events = http
        .get(server.api("/minds/a1/events?after=5"))
        .send()
        .await
        .unwrap();
    let mut stream = String::new();
    while !stream.contains("id: 1199\n") {
        let chunk = timeout(Duration::from_secs(5), events.chunk())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        stream.push_str(std::str::from_utf8(&chunk).unwrap());
    }
    assert_eq!(stream.matches("\nid: ").count() + 1, 1195, "the events");
    assert!(stream.starts_with("id: 5\n"));
}

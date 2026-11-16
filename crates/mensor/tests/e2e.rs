//! A mind's host and real hands, joined over a real WebSocket on loopback or
//! in memory. The model is scripted. Commands run in temporary directories.

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use mensor::hand::Does;
use mensor::local::{InProcess, Local};
use mensor_hand::{Capabilities, Core, Limits};
use mensor_hand_host::capabilities::{Commands, Disk, Files};
use mensor_hand_host::{Backoff, Dial, Error, Handle, WebSocket, boot, start};
use mensor_mind::{
    Block, Content, Item, MemoryStore, MindRecord, ModelReply, ModelRequest, Origin, Outcome, Role,
    Settings, State, Stop, Store, Usage,
};
use mensor_mind_host::{Config, Host, HostError, MindStatus, Prompted, Scripted};
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

    /// Starts a hand in the host's own process that can clone itself: its
    /// clones start in this process too, and are made in `clones`.
    fn cloning_hand(&self, mind: &str, hand: &str, dir: &Path, clones: &Path) -> Handle {
        let does = Does::new(
            dir,
            Some(clones.to_owned()),
            &["commands", "files", "clone"],
        );
        let hands = InProcess::new(self.host.clone(), does.clone());
        hands.start(name(mind), name(hand), &does).unwrap()
    }

    async fn prompt(&self, mind: &str, text: &str) {
        let prompted = self
            .host
            .prompt(&name(mind), None, text.into())
            .await
            .unwrap();
        assert_eq!(prompted, Prompted::Accepted);
    }

    async fn status(&self, mind: &str) -> Option<MindStatus> {
        self.host.mind(&name(mind)).await.ok()
    }

    /// Waits until the mind has these hands connected and is idle.
    async fn settled_with(&self, mind: &str, hands: &[&str]) {
        let hands: Vec<Name> = hands.iter().map(|hand| name(hand)).collect();
        eventually(&format!("{mind} has {hands:?} and is idle"), || async {
            self.status(mind)
                .await
                .is_some_and(|status| status.hands == hands && status.state == State::Idle)
        })
        .await;
    }

    /// Waits until the mind's one hand, `a1`, is connected and it is idle.
    async fn settled(&self, mind: &str) {
        self.settled_with(mind, &["a1"]).await;
    }

    fn history(&self, mind: &str) -> Vec<Item> {
        self.host.history(&name(mind), 0).unwrap()
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

/// How a hand ended, within ten seconds.
async fn ended(task: JoinHandle<Result<(), Error>>) -> Result<(), Error> {
    timeout(Duration::from_secs(10), task)
        .await
        .expect("the hand did not end")
        .expect("the hand's task failed")
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

/// A hand `hand` of the mind `mind`, starting from nothing.
fn fresh(mind: &str, hand: &str) -> Core {
    Core::new(name(mind), name(hand), boot(), Limits::default())
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

fn on(id: &str, hand: &str, command: &str) -> Block {
    Block::ToolUse {
        id: id.into(),
        name: "bash".into(),
        input: json!({"command": command, "hand": hand}),
    }
}

fn own(id: &str, tool: &str, input: Value) -> Block {
    Block::ToolUse {
        id: id.into(),
        name: tool.into(),
        input,
    }
}

/// A reply of these blocks, as a model gives it.
fn reply(content: Vec<Block>) -> ModelReply {
    let tools = content
        .iter()
        .any(|block| matches!(block, Block::ToolUse { .. }));
    ModelReply {
        content,
        stop: if tools { Stop::ToolUse } else { Stop::EndTurn },
        usage: Usage::default(),
    }
}

/// Everything a request tells the model, as one text.
fn told(request: &ModelRequest) -> String {
    let content = request.messages.iter().flat_map(|message| &message.content);
    content
        .map(|content| match content {
            Content::Text(text) | Content::ToolResult { text, .. } => text.clone(),
            Content::ToolUse { input, .. } => input.to_string(),
            Content::Other(value) => value.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
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
    let (_hand, _task) = server.hand(fresh("m", "a1"), dir.path());
    server.settled("m").await;

    server
        .prompt("m", "write a greeting and read it back")
        .await;
    server.settled("m").await;

    assert_eq!(
        std::fs::read_to_string(dir.path().join("greeting.txt")).unwrap(),
        "hello\n"
    );
    let history = server.history("m");
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

    let minds = server.host.minds().await;
    assert_eq!(minds.len(), 1);
    assert_eq!(minds[0].mind.origin, Origin::Root);
}

#[tokio::test]
async fn two_hands_of_one_mind_run_their_calls_at_once() {
    let server = Server::start(Config::default()).await;
    let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (_a1, _a1_task) = server.hand(fresh("m", "a1"), a.path());
    let (_b2, _b2_task) = server.hand(fresh("m", "b2"), b.path());
    server.settled_with("m", &["a1", "b2"]).await;

    let timed = "date +%s%N > start; sleep 0.4; date +%s%N > end";
    server
        .model
        .then(vec![on("tu_1", "a1", timed), on("tu_2", "b2", timed)])
        .then(vec![text("Both ran.")]);
    server.prompt("m", "run in both places").await;
    server.settled_with("m", &["a1", "b2"]).await;

    let time = |dir: &Path, file: &str| -> u128 {
        let text = std::fs::read_to_string(dir.join(file)).unwrap();
        text.trim().parse().unwrap()
    };
    // Each started before the other ended.
    assert!(time(a.path(), "start") < time(b.path(), "end"));
    assert!(time(b.path(), "start") < time(a.path(), "end"));
    assert_eq!(said(&server.history("m")), ["Both ran."]);
}

#[tokio::test]
async fn a_mind_has_as_many_hands_as_it_may() {
    let server = Server::start(Config {
        mind: Settings {
            max_hands: 2,
            ..Settings::default()
        },
        ..Config::default()
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let (_a1, _a1_task) = server.hand(fresh("m", "a1"), dir.path());
    let (_b2, _b2_task) = server.hand(fresh("m", "b2"), dir.path());
    server.settled_with("m", &["a1", "b2"]).await;

    let (_c3, refused) = server.hand(fresh("m", "c3"), dir.path());
    let error = ended(refused).await.unwrap_err();
    assert!(matches!(error, Error::Refused(Refusal::Limit)), "{error}");
}

#[tokio::test]
async fn a_hand_that_dies_mid_call_is_let_go_from_outside_and_the_turn_goes_on() {
    let server = Server::start(Config::default()).await;
    let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a1, _a1_task) = server.hand(fresh("m", "a1"), a.path());
    let (_b2, _b2_task) = server.hand(fresh("m", "b2"), b.path());
    server.settled_with("m", &["a1", "b2"]).await;

    server.model.then(vec![
        on("tu_1", "a1", "echo started > mark; sleep 60"),
        on("tu_2", "b2", "echo fine"),
    ]);
    server.prompt("m", "both").await;
    eventually("a1's command has started", || async {
        a.path().join("mark").exists()
    })
    .await;
    // a1 dies, and never comes back.
    a1.stop();
    eventually("the mind waits for a1", || async {
        server
            .status("m")
            .await
            .is_some_and(|status| status.waiting == [name("a1")] && status.hands == [name("b2")])
    })
    .await;

    server.model.then(vec![text("a1 is gone; b2 said fine.")]);
    let released = client()
        .post(server.api("/minds/m/release"))
        .json(&json!({"hand": "a1"}))
        .send()
        .await
        .unwrap();
    assert_eq!(released.status(), 200);
    server.settled_with("m", &["b2"]).await;

    let history = server.history("m");
    assert!(matches!(outcome(&history, "tu_1"), Outcome::Unknown(why) if why.contains("released")));
    assert_eq!(stdout(outcome(&history, "tu_2")), "fine\n");
    assert_eq!(said(&history), ["a1 is gone; b2 said fine."]);

    // a1 is refused if it comes back, and its process ends quietly.
    let (_back, refused) = server.hand(fresh("m", "a1"), a.path());
    assert!(ended(refused).await.is_ok());
    let unknown = client()
        .post(server.api("/minds/m/release"))
        .json(&json!({"hand": "a1"}))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 404);
}

/// Answers a mind whose model forks once from its hand `a1`, and the child
/// that fork makes: the parent writes a note, forks, and says so; the child
/// adds to the note in its copy and says so.
fn forking(request: &ModelRequest) -> ModelReply {
    let told = told(request);
    reply(if told.contains("\"you\"") {
        if told.contains("CHILD-DONE") {
            vec![text("Child done.")]
        } else {
            vec![bash(
                "tu_c",
                "echo child >> note.txt && cat note.txt && echo CHILD-DONE",
            )]
        }
    } else if told.contains("children") {
        vec![text("Forked.")]
    } else if told.contains("exit") {
        vec![own(
            "tu_f",
            "fork",
            json!({"prompts": ["add a line to the note"]}),
        )]
    } else {
        vec![bash("tu_w", "echo parent > note.txt")]
    })
}

#[tokio::test]
async fn a_fork_makes_a_child_that_works_in_a_copy_of_the_hand() {
    let server = Server::start(Config::default()).await;
    for _ in 0..6 {
        server.model.then_with(forking);
    }
    let root = tempfile::tempdir().unwrap();
    let (dir, clones) = (root.path().join("work"), root.path().join("clones"));
    std::fs::create_dir(&dir).unwrap();
    let _a1 = server.cloning_hand("m", "a1", &dir, &clones);
    server.settled("m").await;

    server.prompt("m", "write a note, then fork").await;
    eventually("the child exists and is done", || async {
        let minds = server.host.minds().await;
        minds.len() == 2
            && minds
                .iter()
                .all(|status| status.state == State::Idle && status.hands == [name("a1")])
    })
    .await;

    let minds = server.host.minds().await;
    let child = minds
        .iter()
        .find(|status| status.mind.name != name("m"))
        .unwrap();
    let child_name = child.mind.name.clone();
    assert!(child_name.as_str().starts_with("m."), "{child_name}");
    let parent = server.history("m");
    let Origin::Fork {
        parent: of,
        inherited,
    } = &child.mind.origin
    else {
        panic!("not a fork: {:?}", child.mind.origin);
    };
    assert_eq!(of, &name("m"));
    let inherited = usize::try_from(*inherited).unwrap();
    assert!(
        matches!(&parent[inherited - 1], Item::Call { capability, .. } if capability == "clone")
    );

    // The parent is told its children, and carries on.
    assert_eq!(
        outcome(&parent, "tu_f"),
        &Outcome::Output(json!({"children": [child_name]}))
    );
    assert_eq!(said(&parent), ["Forked."]);

    // The child shares the parent's history up to the clone, then works in
    // its own copy of the hand's directory.
    let history = server.history(child_name.as_str());
    assert_eq!(history[..inherited], parent[..inherited]);
    assert_eq!(said(&history), ["Child done."]);
    assert_eq!(
        std::fs::read_to_string(dir.join("note.txt")).unwrap(),
        "parent\n"
    );
    let copy = clones.join(child_name.as_str()).join("a1");
    assert_eq!(
        std::fs::read_to_string(copy.join("note.txt")).unwrap(),
        "parent\nchild\n"
    );
}

/// Answers a mind whose model clones `local` into itself, runs a command in
/// both, then lets the clone go.
fn cloning_into_itself(request: &ModelRequest) -> ModelReply {
    let told = told(request);
    reply(if told.contains("\"released\"") {
        vec![text("Done with the clone.")]
    } else if told.contains("BOTH") {
        vec![own("tu_r", "release_hand", json!({"hand": "local-2"}))]
    } else if told.contains("local-2") {
        vec![
            on("tu_a", "local", "echo BOTH > where.txt; pwd"),
            on("tu_b", "local-2", "echo BOTH > where.txt; pwd"),
        ]
    } else {
        vec![own("tu_c", "clone_hand", json!({}))]
    })
}

#[tokio::test]
async fn a_clone_joins_its_mind_works_beside_its_original_and_is_let_go() {
    let server = Server::start(Config::default()).await;
    for _ in 0..5 {
        server.model.then_with(cloning_into_itself);
    }
    let root = tempfile::tempdir().unwrap();
    let (dir, clones) = (root.path().join("work"), root.path().join("clones"));
    std::fs::create_dir(&dir).unwrap();
    let _local = server.cloning_hand("main", "local", &dir, &clones);
    server.settled_with("main", &["local"]).await;

    server.prompt("main", "work in two places").await;
    eventually("the turn is over", || async {
        server
            .status("main")
            .await
            .is_some_and(|status| status.state == State::Idle && status.hands == [name("local")])
            && said(&server.history("main")) == ["Done with the clone."]
    })
    .await;

    let history = server.history("main");
    assert_eq!(
        outcome(&history, "tu_c"),
        &Outcome::Output(json!({"hand": "local-2"}))
    );
    let copy = clones.join("main").join("local-2");
    assert!(stdout(outcome(&history, "tu_b")).contains("local-2"));
    assert_eq!(
        std::fs::read_to_string(dir.join("where.txt")).unwrap(),
        "BOTH\n"
    );
    assert_eq!(
        std::fs::read_to_string(copy.join("where.txt")).unwrap(),
        "BOTH\n"
    );
    assert!(
        history
            .iter()
            .any(|item| matches!(item, Item::Released { hand, .. } if *hand == name("local-2")))
    );
}

#[tokio::test]
async fn only_a_parent_makes_a_mind_with_a_child_name() {
    let server = Server::start(Config::default()).await;
    let dir = tempfile::tempdir().unwrap();
    // A hand that dials a child its parent has not made yet dials again,
    // and starts no mind of that name.
    let (_early, waiting) = server.hand(fresh("m.k3x9q2ab", "a1"), dir.path());
    sleep(Duration::from_millis(150)).await;
    assert!(
        !waiting.is_finished(),
        "a child not made yet is worth another dial"
    );
    assert!(server.host.minds().await.is_empty());

    // A name too long to have children is refused for good.
    let long = "x".repeat(Name::ROOT_MAX_LEN + 1);
    let (_long, refused) = server.hand(fresh(&long, "a1"), dir.path());
    let error = ended(refused).await.unwrap_err();
    assert!(matches!(error, Error::Refused(Refusal::Name)), "{error}");
}

#[tokio::test]
async fn a_dropped_connection_loses_no_result() {
    let server = Server::start(Config::default()).await;
    server
        .model
        .then(vec![bash(
            "tu_1",
            "echo started > mark; sleep 0.3; echo survived",
        )])
        .then(vec![text("Done.")]);
    let dir = tempfile::tempdir().unwrap();
    let (hand, _task) = server.hand(fresh("m", "a1"), dir.path());
    server.settled("m").await;
    server.prompt("m", "wait, then speak").await;
    eventually("the command is running", || async {
        dir.path().join("mark").exists()
    })
    .await;

    // The command outlives the connection, and its result arrives on the
    // next one.
    hand.redial();
    server.settled("m").await;

    let history = server.history("m");
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
async fn a_host_that_restarts_carries_every_mind_on() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mensor.sqlite");
    let open = || -> Arc<dyn Store> { Arc::new(SqliteStore::open(&path).unwrap()) };

    let first = Server::over(open(), Config::default(), "127.0.0.1:0").await;
    first
        .model
        .then(vec![bash("tu_1", "echo one")])
        .then(vec![text("One.")]);
    let (hand, _task) = first.hand(fresh("m", "a1"), dir.path());
    first.settled("m").await;
    first.prompt("m", "count").await;
    first.settled("m").await;
    let address = first.address.to_string();
    let before = first.history("m");
    first.stop().await;

    // A new host over the same file, on the same address. The hand drops
    // its old connection and finds the new host.
    let second = Server::over(open(), Config::default(), &address).await;
    assert_eq!(second.history("m"), before);
    hand.redial();
    second.settled("m").await;

    second
        .model
        .then(vec![bash("tu_2", "echo two")])
        .then(vec![text("Two.")]);
    second.prompt("m", "count again").await;
    second.settled("m").await;
    let history = second.history("m");
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
async fn the_api_prompts_a_mind_and_follows_its_history() {
    let server = Server::start(Config::default()).await;
    server.model.then(vec![text("Hello.")]);
    let dir = tempfile::tempdir().unwrap();
    let (_hand, _task) = server.hand(fresh("m", "a1"), dir.path());
    server.settled("m").await;
    let http = client();
    let prompt = |body: Value| http.post(server.api("/minds/m/prompt")).json(&body).send();

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
    server.settled("m").await;

    let minds: Value = http
        .get(server.api("/minds"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        minds,
        json!([{"name": "m", "origin": "root", "state": "idle", "hands": ["a1"],
                "waiting": [], "length": 3}])
    );

    let history: Value = http
        .get(server.api("/minds/m/history?after=1"))
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
        .get(server.api("/minds/m/events?after=1"))
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
    server.prompt("m", "once more").await;
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
        server.host.mind(&name("nobody")).await,
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

    let (_stranger, refused) = server.hand_with(fresh("x", "a1"), dir.path(), Some("wrong"));
    let error = ended(refused).await.unwrap_err();
    assert!(matches!(error, Error::Unauthorized), "{error}");
    assert!(server.host.minds().await.is_empty());

    let (_hand, _task) = server.hand_with(fresh("m", "a1"), dir.path(), Some("secret"));
    server.settled("m").await;
}

#[tokio::test]
async fn a_hand_in_the_same_process_and_one_across_a_socket_serve_one_mind() {
    let server = Server::start(Config::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let (_local, _local_task) = server.local_hand(fresh("main", "local"), dir.path());
    let (_remote, _remote_task) = server.hand(fresh("main", "remote"), dir.path());
    server.settled_with("main", &["local", "remote"]).await;

    server
        .model
        .then(vec![
            on("tu_1", "local", "echo 1"),
            on("tu_2", "remote", "echo 2"),
        ])
        .then(vec![text("1 and 2.")]);
    server.prompt("main", "run both").await;
    server.settled_with("main", &["local", "remote"]).await;
    let history = server.history("main");
    assert_eq!(stdout(outcome(&history, "tu_1")), "1\n");
    assert_eq!(stdout(outcome(&history, "tu_2")), "2\n");
    assert_eq!(said(&history), ["1 and 2."]);
}

#[tokio::test]
async fn a_newer_connection_for_a_hand_takes_the_older_ones_place() {
    let server = Server::start(Config::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let (_first, replaced) = server.local_hand(fresh("main", "local"), dir.path());
    server.settled_with("main", &["local"]).await;

    let (_usurper, _usurper_task) = server.local_hand(fresh("main", "local"), dir.path());
    let error = ended(replaced).await.unwrap_err();
    assert!(
        matches!(error, Error::Refused(Refusal::Replaced)),
        "{error}"
    );
}

#[tokio::test]
async fn an_interrupt_kills_the_command_a_hand_runs_and_ends_the_turn() {
    let server = Server::start(Config::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let (_hand, _task) = server.hand(fresh("m", "a1"), dir.path());
    server.settled("m").await;

    server.model.then(vec![bash(
        "tu_1",
        "echo started > mark; sleep 60; echo finished > end",
    )]);
    server.prompt("m", "Wait a minute.").await;
    eventually("the command has started", || async {
        dir.path().join("mark").exists()
    })
    .await;

    let interrupted = client()
        .post(server.api("/minds/m/interrupt"))
        .send()
        .await
        .unwrap();
    assert_eq!(interrupted.status(), 200);
    server.settled("m").await;

    let items = server.history("m");
    assert_eq!(outcome(&items, "tu_1"), &Outcome::Error("cancelled".into()));
    assert!(items.contains(&Item::Paused {
        reason: "interrupted".into()
    }));
    assert!(!dir.path().join("end").exists());
    // The cancelled call was not handed to the model: the turn is over.
    assert_eq!(server.model.requests().len(), 1);

    server.model.then(vec![text("Stopped.")]);
    server.prompt("m", "Never mind.").await;
    server.settled("m").await;
    assert_eq!(said(&server.history("m")), ["Stopped."]);
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
    let (_hand, _task) = server.hand(fresh("m", "a1"), dir.path());
    server.settled("m").await;

    server.model.then_with(|_| ModelReply {
        content: vec![text("hi")],
        stop: Stop::EndTurn,
        usage: Usage {
            input: 95,
            output: 5,
        },
    });
    server.prompt("m", "hello").await;
    server.settled("m").await;

    server
        .model
        .then(vec![text("We greeted each other.")])
        .then(vec![text("Paris.")]);
    server.prompt("m", "What is the capital of France?").await;
    server.settled("m").await;

    let items = server.history("m");
    assert!(items.iter().any(|item| matches!(
        item,
        Item::Summary { text, .. } if text == "We greeted each other."
    )));
    assert_eq!(said(&items), ["hi", "Paris."]);
    let requests = server.model.requests();
    assert!(requests[1].text_only);
    // The step after the summary reads the summary, the hands, and the
    // prompt, and nothing of what came before.
    assert_eq!(requests[2].messages.len(), 1);
    assert_eq!(requests[2].messages[0].content.len(), 3);
}

/// A host over a database file whose minds rest soon, and sum up early.
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

/// Stops a hand, and waits until its mind has left the host's memory.
async fn rested(server: &Server, hand: &Handle) {
    hand.stop();
    eventually("the mind rests", || async {
        server.host.awake().await == 0
    })
    .await;
}

#[tokio::test]
async fn a_mind_with_nothing_to_do_rests_in_the_store_and_wakes_when_needed() {
    let dir = tempfile::tempdir().unwrap();
    let server = restful(&dir.path().join("mensor.sqlite")).await;
    let m = name("m");
    let (hand, _task) = server.hand(fresh("m", "a1"), dir.path());
    server.settled("m").await;
    server.model.then_with(|_| ModelReply {
        content: vec![text("hi")],
        stop: Stop::EndTurn,
        usage: Usage {
            input: 95,
            output: 5,
        },
    });
    let keyed = |text: &str| server.host.prompt(&m, Some("k1".into()), text.into());
    assert_eq!(keyed("hello").await.unwrap(), Prompted::Accepted);
    server.settled("m").await;

    // A mind whose hand is connected stays awake, however long it idles.
    sleep(Duration::from_millis(200)).await;
    assert_eq!(server.host.awake().await, 1);

    // One with no hand, no follower and no model asked leaves memory. What
    // it is doing is then read from the store, and wakes nothing.
    rested(&server, &hand).await;
    let status = server.host.mind(&m).await.unwrap();
    assert_eq!(
        (status.state, status.hands.len(), status.length),
        (State::Idle, 0, 3)
    );
    assert_eq!(server.host.minds().await.len(), 1);
    assert_eq!(server.history("m").len(), 3);
    assert_eq!(server.host.awake().await, 0);

    // A prompt wakes it for as long as it takes to store. With no hand the
    // mind asks nobody, and rests again mid-turn.
    server.prompt("m", "What is the capital of France?").await;
    eventually("the mind rests again", || async {
        server.host.awake().await == 0
    })
    .await;
    let status = server.host.mind(&m).await.unwrap();
    assert_eq!((status.state, status.length), (State::Thinking, 4));
    assert_eq!(server.model.requests().len(), 1);

    // A hand wakes it, and the turn carries on: here by summing up.
    server
        .model
        .then(vec![text("We greeted each other.")])
        .then(vec![text("Paris.")]);
    let (hand, _task) = server.hand(fresh("m", "a1"), dir.path());
    server.settled("m").await;
    assert_eq!(said(&server.history("m")), ["hi", "Paris."]);

    // Woken again, the mind is given only the end of its history: from
    // the summary. A key from before it is found in the store.
    rested(&server, &hand).await;
    assert_eq!(keyed("hello").await.unwrap(), Prompted::Existing);
    assert!(matches!(
        keyed("goodbye").await,
        Err(HostError::Conflict(_))
    ));
    server.model.then(vec![text("Lyon is not.")]);
    let (_hand, _task) = server.hand(fresh("m", "a1"), dir.path());
    server.settled("m").await;
    server.prompt("m", "And Lyon?").await;
    server.settled("m").await;
    let requests = server.model.requests();
    let asked = &requests.last().unwrap().messages;
    assert_eq!(asked.len(), 3);
    assert!(matches!(
        &asked[0].content[0],
        Content::Text(summary) if summary.contains("We greeted each other.")
    ));
    assert_eq!(said(&server.history("m")), ["hi", "Paris.", "Lyon is not."]);
}

#[tokio::test]
async fn a_mind_that_is_followed_stays_awake() {
    let dir = tempfile::tempdir().unwrap();
    let server = restful(&dir.path().join("mensor.sqlite")).await;
    let (hand, _task) = server.hand(fresh("m", "a1"), dir.path());
    server.settled("m").await;
    let events = client()
        .get(server.api("/minds/m/events"))
        .send()
        .await
        .unwrap();
    hand.stop();
    sleep(Duration::from_millis(200)).await;
    assert_eq!(server.host.awake().await, 1);

    drop(events);
    // A stream that is dropped is noticed when the host next writes to it.
    server.prompt("m", "anyone?").await;
    eventually("the mind rests", || async {
        server.host.awake().await == 0
    })
    .await;
}

#[tokio::test]
async fn a_long_history_is_read_a_page_at_a_time() {
    let store = Arc::new(MemoryStore::new());
    let prompts: Vec<Item> = (0..1200)
        .map(|index| Item::Prompt {
            key: None,
            text: format!("prompt {index}"),
        })
        .collect();
    let record = MindRecord {
        name: name("m"),
        origin: Origin::Root,
    };
    store.create(&record, &prompts).unwrap();
    let server = Server::over(store, Config::default(), "127.0.0.1:0").await;
    let http = client();

    // One answer holds a page, and says where the next begins.
    let mut next = 0;
    let mut read = 0;
    loop {
        let history: Value = http
            .get(server.api(&format!("/minds/m/history?after={next}")))
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
        .get(server.api("/minds/m/events?after=5"))
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

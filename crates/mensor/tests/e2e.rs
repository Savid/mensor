//! A mind's host and real hands, joined over a real WebSocket on loopback or
//! in memory. The model is scripted. Commands run in temporary directories.

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use mensor::hand::{Does, Hands};
use mensor::local::Local;
use mensor_hand::{Capabilities, Core, Limits};
use mensor_hand_host::capabilities::{Commands, Disk, Files};
use mensor_hand_host::{Backoff, Dial, Ended, Error, Handle, WebSocket, boot, start};
use mensor_mind::{
    Block, Content, Item, MemoryStore, MindRecord, ModelReply, ModelRequest, Origin, Outcome, Role,
    Settings, State, Stop, Store, StoreError, Usage,
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
        let host = Host::open(store, model.clone(), config).await.unwrap();
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
    fn hand(&self, core: Core, dir: &Path) -> (Handle, JoinHandle<Result<Ended, Error>>) {
        self.hand_with(core, dir, None)
    }

    fn hand_with(
        &self,
        core: Core,
        dir: &Path,
        token: Option<&str>,
    ) -> (Handle, JoinHandle<Result<Ended, Error>>) {
        let dial = WebSocket::new(self.attach()).token(token.map(str::to_owned));
        hand_through(dial, core, dir)
    }

    /// Starts a hand in the host's own process, with no socket between them.
    fn local_hand(&self, core: Core, dir: &Path) -> (Handle, JoinHandle<Result<Ended, Error>>) {
        hand_through(Local::new(self.host.clone()), core, dir)
    }

    /// Starts a hand in the host's own process that can clone itself: its
    /// clones start in this process too, and are made in `clones`.
    fn cloning_hand(&self, mind: &str, hand: &str, dir: &Path, clones: &Path) -> Hands {
        let does = Does::new(
            dir,
            Some(clones.to_owned()),
            &["commands", "files", "clone"],
        );
        let hands = Hands::new(Local::new(self.host.clone()), does);
        hands.start(name(mind), name(hand)).unwrap();
        hands
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
) -> (Handle, JoinHandle<Result<Ended, Error>>) {
    let capabilities = Capabilities::new()
        .with(Commands::new(dir))
        .with(Files::new(Disk::new(dir)));
    let mut config = mensor_hand_host::Config::new(dial, capabilities);
    config.backoff = Backoff {
        min: Duration::from_millis(10),
        max: Duration::from_millis(50),
    };
    config.give_up = Duration::from_millis(500);
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
async fn ended(task: JoinHandle<Result<Ended, Error>>) -> Result<Ended, Error> {
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

/// The one directory in `dir`: a clone's copy, `<hand>.<code>`.
fn only_in(dir: &Path) -> std::path::PathBuf {
    let copies: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .collect();
    assert_eq!(copies.len(), 1, "{copies:?}");
    copies.into_iter().next().unwrap()
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
    assert_eq!(ended(refused).await.unwrap(), Ended::Released);
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
    let copy = only_in(&clones.join(child_name.as_str()));
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
            on("tu_b", "local-2", "echo BOTH > where.txt; ls; pwd"),
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
    // The clone worked in its own copy of the directory.
    let there = stdout(outcome(&history, "tu_b"));
    assert!(
        there.contains("where.txt") && there.contains("main/local-2."),
        "{there}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("where.txt")).unwrap(),
        "BOTH\n"
    );
    assert!(
        history
            .iter()
            .any(|item| matches!(item, Item::Released { hand, .. } if *hand == name("local-2")))
    );
    // Let go, the clone is done with its copy.
    eventually("the clone's copy is removed", || async {
        std::fs::read_dir(clones.join("main")).is_ok_and(|mut copies| copies.next().is_none())
    })
    .await;
}

#[tokio::test]
async fn only_a_parent_makes_a_mind_with_a_child_name() {
    let server = Server::start(Config::default()).await;
    let dir = tempfile::tempdir().unwrap();
    // A hand that dials a child its parent has not made yet dials again,
    // starts no mind of that name, and gives up in the end.
    let started = Instant::now();
    let (_early, waiting) = server.hand(fresh("m.d3342iuj", "a1"), dir.path());
    let error = ended(waiting).await.unwrap_err();
    assert!(
        matches!(error, Error::Refused(Refusal::UnknownMind)),
        "{error}"
    );
    assert!(started.elapsed() >= Duration::from_millis(500));
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
                "waiting": [], "watching": [], "length": 3}])
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
            from: None,
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

/// The name a fork's outcome gives its first child, in what a model is told.
fn first_child(told: &str) -> String {
    let at = told.find("\"children\":[\"").expect("no fork was told") + "\"children\":[\"".len();
    told[at..].split('"').next().unwrap().to_owned()
}

/// Answers a parent that forks one child, waits for it, tells it more and
/// waits again, and the child that does each thing it is told in its copy.
fn talking(request: &ModelRequest) -> ModelReply {
    let told = told(request);
    reply(if told.contains("\"you\"") {
        if told.contains("now write B") {
            if told.contains("WROTE-B") {
                vec![text("B-DONE")]
            } else {
                vec![bash("tu_cb", "echo B > b.txt && printf 'WR%s' OTE-B")]
            }
        } else if told.contains("WROTE-A") {
            vec![text("A-DONE")]
        } else {
            vec![bash("tu_ca", "echo A > a.txt && printf 'WR%s' OTE-A")]
        }
    } else if told.contains("B-DONE") {
        vec![text("All done.")]
    } else if told.contains("A-DONE") {
        let child = first_child(&told);
        vec![
            own(
                "tu_t",
                "tell",
                json!({"mind": child, "text": "now write B"}),
            ),
            own("tu_w2", "wait", json!({})),
        ]
    } else if told.contains("\"children\"") {
        vec![own("tu_w1", "wait", json!({}))]
    } else {
        vec![own("tu_f", "fork", json!({"prompts": ["write A"]}))]
    })
}

#[tokio::test]
async fn a_parent_waits_for_its_child_and_tells_it_more() {
    let server = Server::start(Config::default()).await;
    for _ in 0..8 {
        server.model.then_with(talking);
    }
    let root = tempfile::tempdir().unwrap();
    let (dir, clones) = (root.path().join("work"), root.path().join("clones"));
    std::fs::create_dir(&dir).unwrap();
    let _hands = server.cloning_hand("m", "a1", &dir, &clones);
    server.settled("m").await;

    server.prompt("m", "split the work").await;
    eventually("the parent is done", || async {
        said(&server.history("m")) == ["All done."]
    })
    .await;

    let parent = server.history("m");
    let child = Name::new(first_child(
        &serde_json::to_string(&parent).unwrap().replace(' ', ""),
    ))
    .unwrap();
    assert_eq!(
        outcome(&parent, "tu_w1"),
        &Outcome::Output(json!({child.as_str(): {"said": "A-DONE"}}))
    );
    assert_eq!(
        outcome(&parent, "tu_t"),
        &Outcome::Output(json!({"told": child}))
    );
    assert_eq!(
        outcome(&parent, "tu_w2"),
        &Outcome::Output(json!({child.as_str(): {"said": "B-DONE"}}))
    );
    let history = server.history(child.as_str());
    assert!(history.iter().any(|item| matches!(
        item,
        Item::Prompt { text, from: Some(from), .. } if text == "now write B" && *from == name("m")
    )));
    let copy = only_in(&clones.join(child.as_str()));
    assert!(copy.join("a.txt").exists() && copy.join("b.txt").exists());
    assert!(!dir.join("a.txt").exists());
}

#[tokio::test]
async fn clones_start_again_with_their_hands_and_are_let_go_with_their_copies() {
    let server = Server::start(Config::default()).await;
    server
        .model
        .then(vec![own("tu_c", "clone_hand", json!({}))])
        .then(vec![text("Cloned.")]);
    let root = tempfile::tempdir().unwrap();
    let (dir, clones) = (root.path().join("work"), root.path().join("clones"));
    std::fs::create_dir(&dir).unwrap();
    let hands = server.cloning_hand("main", "local", &dir, &clones);
    server.settled_with("main", &["local"]).await;
    server.prompt("main", "clone yourself").await;
    server.settled_with("main", &["local", "local-2"]).await;

    // Stopped, the hands end and the clone's copy is kept.
    hands.stop();
    hands.ended().await.unwrap();
    server.settled_with("main", &[]).await;
    let copy = only_in(&clones.join("main"));

    // Started again, the clone starts with its original.
    let again = server.cloning_hand("main", "local", &dir, &clones);
    server.settled_with("main", &["local", "local-2"]).await;
    assert_eq!(only_in(&clones.join("main")), copy);

    server
        .host
        .release(&name("main"), name("local-2"))
        .await
        .unwrap();
    eventually("the clone's copy is removed", || async { !copy.exists() }).await;
    server.settled_with("main", &["local"]).await;

    // The original let go, the hands have all ended.
    server
        .host
        .release(&name("main"), name("local"))
        .await
        .unwrap();
    timeout(Duration::from_secs(10), again.ended())
        .await
        .expect("the hands did not end")
        .unwrap();
    assert!(dir.exists(), "the original's directory is never removed");
}

/// A store that fails the first append after it first makes a fork's
/// child, as a host would that stopped between the two.
struct StopsAfterFork {
    inner: MemoryStore,
    armed: std::sync::atomic::AtomicBool,
    fired: std::sync::atomic::AtomicBool,
}

impl Store for StopsAfterFork {
    fn create(&self, mind: &MindRecord, items: &[Item]) -> Result<(), StoreError> {
        self.inner.create(mind, items)?;
        if matches!(mind.origin, Origin::Fork { .. })
            && !self.fired.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            self.armed.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        Ok(())
    }

    fn append(&self, name: &Name, items: &[Item]) -> Result<(), StoreError> {
        if self.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return Err(StoreError::Backend("the disk went away".into()));
        }
        self.inner.append(name, items)
    }

    fn read(&self, name: &Name, from: u64, limit: usize) -> Result<Vec<Item>, StoreError> {
        self.inner.read(name, from, limit)
    }

    fn last(&self, name: &Name, kind: &str) -> Result<Option<Item>, StoreError> {
        self.inner.last(name, kind)
    }

    fn prompt(&self, name: &Name, key: &str) -> Result<Option<String>, StoreError> {
        self.inner.prompt(name, key)
    }

    fn minds(&self) -> Result<Vec<MindRecord>, StoreError> {
        self.inner.minds()
    }
}

#[tokio::test]
async fn a_fork_its_host_stopped_in_is_made_once() {
    let store = Arc::new(StopsAfterFork {
        inner: MemoryStore::new(),
        armed: std::sync::atomic::AtomicBool::new(false),
        fired: std::sync::atomic::AtomicBool::new(false),
    });
    let server = Server::over(store, Config::default(), "127.0.0.1:0").await;
    for _ in 0..3 {
        server.model.then_with(|request| {
            let told = told(request);
            reply(if told.contains("\"you\"") {
                vec![text("Child.")]
            } else if told.contains("\"children\"") {
                vec![text("Forked.")]
            } else {
                vec![own("tu_f", "fork", json!({"prompts": ["go"]}))]
            })
        });
    }
    let root = tempfile::tempdir().unwrap();
    let (dir, clones) = (root.path().join("work"), root.path().join("clones"));
    std::fs::create_dir(&dir).unwrap();
    let _hands = server.cloning_hand("m", "a1", &dir, &clones);
    server.settled("m").await;

    // The fork is stored only when the hand, dialling again, gives its
    // result once more.
    server.prompt("m", "fork").await;
    eventually("both minds are done", || async {
        let minds = server.host.minds().await;
        minds.len() == 2 && minds.iter().all(|status| status.state == State::Idle)
    })
    .await;
    let parent = server.history("m");
    assert_eq!(said(&parent), ["Forked."]);
    let forks = parent
        .iter()
        .filter(|item| matches!(item, Item::Forked { .. }))
        .count();
    assert_eq!(forks, 1);
    let minds = server.host.minds().await;
    let child = &minds
        .iter()
        .find(|status| status.mind.name != name("m"))
        .unwrap()
        .mind
        .name;
    assert_eq!(said(&server.history(child.as_str())), ["Child."]);
    assert_eq!(
        std::fs::read_dir(clones.join(child.as_str()))
            .unwrap()
            .count(),
        1
    );
}

#[tokio::test]
async fn a_hand_that_restarts_makes_only_its_own_call_unknown() {
    let server = Server::start(Config::default()).await;
    let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a1, _a1_task) = server.hand(fresh("m", "a1"), a.path());
    let (_b2, _b2_task) = server.hand(fresh("m", "b2"), b.path());
    server.settled_with("m", &["a1", "b2"]).await;

    server
        .model
        .then(vec![
            on("tu_1", "a1", "echo started > mark; sleep 30"),
            on("tu_2", "b2", "echo fine"),
        ])
        .then(vec![text("a1 forgot; b2 said fine.")]);
    server.prompt("m", "both").await;
    eventually("a1's command has started", || async {
        a.path().join("mark").exists()
    })
    .await;
    // a1's process restarts: a new boot, remembering nothing.
    a1.stop();
    let (_again, _again_task) = server.hand(fresh("m", "a1"), a.path());
    server.settled_with("m", &["a1", "b2"]).await;

    let history = server.history("m");
    assert!(
        matches!(outcome(&history, "tu_1"), Outcome::Unknown(why) if why.contains("restarted")),
        "{:?}",
        outcome(&history, "tu_1")
    );
    assert_eq!(stdout(outcome(&history, "tu_2")), "fine\n");
    let boots = history
        .iter()
        .filter(|item| matches!(item, Item::Attached { hand, .. } if *hand == name("a1")))
        .count();
    assert_eq!(boots, 2);
    assert_eq!(said(&history), ["a1 forgot; b2 said fine."]);
}

#[tokio::test]
async fn a_mind_with_two_hands_rests_and_wakes_with_both() {
    let dir = tempfile::tempdir().unwrap();
    let server = restful(&dir.path().join("mensor.sqlite")).await;
    let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a1, _a1_task) = server.hand(fresh("m", "a1"), a.path());
    let (b2, _b2_task) = server.hand(fresh("m", "b2"), b.path());
    server.settled_with("m", &["a1", "b2"]).await;

    a1.stop();
    rested(&server, &b2).await;
    let status = server.host.mind(&name("m")).await.unwrap();
    assert_eq!((status.state, status.hands.len()), (State::Idle, 0));

    let (_a1, _a1_task) = server.hand(fresh("m", "a1"), a.path());
    let (_b2, _b2_task) = server.hand(fresh("m", "b2"), b.path());
    server.settled_with("m", &["a1", "b2"]).await;
    server
        .model
        .then(vec![on("tu_1", "a1", "echo a"), on("tu_2", "b2", "echo b")])
        .then(vec![text("Both answered.")]);
    server.prompt("m", "both").await;
    server.settled_with("m", &["a1", "b2"]).await;
    let history = server.history("m");
    assert_eq!(
        (
            stdout(outcome(&history, "tu_1")),
            stdout(outcome(&history, "tu_2"))
        ),
        ("a\n", "b\n")
    );
}

/// Answers a parent that forks and waits, and a child that answers once
/// it has run a command, or once the command is lost: a child whose hand
/// is gone waits with its turn not over.
fn waiting_through(request: &ModelRequest) -> ModelReply {
    let told = told(request);
    reply(if told.contains("\"you\"") {
        if told.contains("restarted") {
            vec![text("Here.")]
        } else {
            vec![bash("tu_s", "sleep 30")]
        }
    } else if told.contains("Here.") {
        vec![text("It answered.")]
    } else if told.contains("\"children\"") {
        vec![own("tu_w", "wait", json!({}))]
    } else {
        vec![own("tu_f", "fork", json!({"prompts": ["take your time"]}))]
    })
}

#[tokio::test]
async fn a_parent_that_waits_through_a_host_restart_hears_from_its_child() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("mensor.sqlite");
    let open = || -> Arc<dyn Store> { Arc::new(SqliteStore::open(&path).unwrap()) };
    let first = Server::over(open(), Config::default(), "127.0.0.1:0").await;
    for _ in 0..3 {
        first.model.then_with(waiting_through);
    }
    let (dir, clones) = (root.path().join("work"), root.path().join("clones"));
    std::fs::create_dir(&dir).unwrap();
    let hands = first.cloning_hand("m", "a1", &dir, &clones);
    first.settled("m").await;
    first.prompt("m", "fork and wait").await;
    let child = std::cell::OnceCell::new();
    eventually(
        "the parent waits for a child that runs a command",
        || async {
            let minds = first.host.minds().await;
            let parent = minds.iter().find(|status| status.mind.name == name("m"));
            let working = minds
                .iter()
                .find(|status| status.mind.name != name("m") && status.state == State::Working);
            match (parent, working) {
                (Some(parent), Some(working)) if parent.watching == [working.mind.name.clone()] => {
                    let _ = child.set(working.mind.name.clone());
                    true
                }
                _ => false,
            }
        },
    )
    .await;
    let child = child.into_inner().unwrap();
    hands.stop();
    hands.ended().await.unwrap();
    first.stop().await;

    // A new host wakes the parent, which waits with no hand. Only the
    // child's hand comes back: its command is lost, and it answers.
    let second = Server::over(open(), Config::default(), "127.0.0.1:0").await;
    eventually("the parent is awake", || async {
        second.host.awake().await == 1
    })
    .await;
    for _ in 0..2 {
        second.model.then_with(waiting_through);
    }
    let copy = only_in(&clones.join(child.as_str()));
    let (_child_hand, _child_task) = second.hand(fresh(child.as_str(), "a1"), &copy);
    eventually(
        "the parent hears from its child with no hand of its own",
        || async {
            second
                .history("m")
                .iter()
                .any(|item| matches!(item, Item::Outcome { tool_use, .. } if tool_use == "tu_w"))
        },
    )
    .await;
    assert_eq!(
        outcome(&second.history("m"), "tu_w"),
        &Outcome::Output(json!({child.as_str(): {"said": "Here."}}))
    );
    let status = second.status("m").await.unwrap();
    assert_eq!((status.state, status.hands.len()), (State::Thinking, 0));

    // Its hand back, it answers.
    let (_hand, _task) = second.hand(fresh("m", "a1"), &dir);
    eventually("the parent answers", || async {
        said(&second.history("m")) == ["It answered."]
    })
    .await;
}

/// Answers a parent that forks and waits, and a child that takes a while.
fn slow_child(request: &ModelRequest) -> ModelReply {
    let told = told(request);
    reply(if told.contains("\"you\"") {
        if told.contains("exit") {
            vec![text("Here.")]
        } else {
            vec![bash("tu_s", "sleep 3")]
        }
    } else if told.contains("Here.") {
        vec![text("It answered.")]
    } else if told.contains("\"children\"") {
        vec![own("tu_w", "wait", json!({}))]
    } else {
        vec![own("tu_f", "fork", json!({"prompts": ["take your time"]}))]
    })
}

#[tokio::test]
async fn a_parent_that_rests_while_it_waits_hears_from_its_child() {
    let server = Server::start(Config {
        rest_after: Duration::from_millis(50),
        ..Config::default()
    })
    .await;
    for _ in 0..5 {
        server.model.then_with(slow_child);
    }
    let root = tempfile::tempdir().unwrap();
    let (dir, clones) = (root.path().join("work"), root.path().join("clones"));
    std::fs::create_dir(&dir).unwrap();
    let _hands = server.cloning_hand("m", "a1", &dir, &clones);
    server.settled("m").await;
    server.prompt("m", "fork and wait").await;
    eventually("the parent waits", || async {
        server
            .status("m")
            .await
            .is_some_and(|status| !status.watching.is_empty())
    })
    .await;

    // The parent lets its hand go, and rests while its child works. Told
    // from the store, it still says whom it waits for.
    server.host.release(&name("m"), name("a1")).await.unwrap();
    let minds = server.host.minds().await;
    let child = &minds
        .iter()
        .find(|status| status.mind.name != name("m"))
        .unwrap()
        .mind
        .name;
    eventually("the child works, and is the only mind awake", || async {
        let working = server
            .status(child.as_str())
            .await
            .is_some_and(|status| status.state == State::Working && status.hands == [name("a1")]);
        working && server.host.awake().await == 1
    })
    .await;
    let parent = server.status("m").await.unwrap();
    assert_eq!(
        (parent.state, parent.watching, parent.hands.len()),
        (State::Working, vec![child.clone()], 0)
    );

    // The child rests; the parent wakes, hears it, and answers.
    eventually("the parent answers", || async {
        said(&server.history("m")) == ["It answered."]
    })
    .await;
    assert_eq!(
        outcome(&server.history("m"), "tu_w"),
        &Outcome::Output(json!({child.as_str(): {"said": "Here."}}))
    );
}

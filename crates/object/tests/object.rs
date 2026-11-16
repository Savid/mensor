//! An object driven as a host drives it, over storage that outlives it.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};

use mensor_hand::{Capabilities, FileSystem, Files, FsFuture};
use mensor_mind::{
    Block, Content, Item, ModelError, ModelReply, ModelRequest, Outcome, Settings, State, Stop,
    Usage,
};
use mensor_object::{MemoryStorage, Model, Object, PromptError, Work, rows, step};
use mensor_protocol::Name;
use serde_json::{Value, json};

/// Runs a future whose every await is ready at once, as all are here.
fn now<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
    }
}

/// A model that answers from a script and remembers what it was asked.
#[derive(Default)]
struct Script {
    replies: RefCell<VecDeque<ModelReply>>,
    requests: RefCell<Vec<ModelRequest>>,
}

impl Script {
    fn then(&self, content: Vec<Block>) -> &Self {
        let uses_tools = content
            .iter()
            .any(|block| matches!(block, Block::ToolUse { .. }));
        self.replies.borrow_mut().push_back(ModelReply {
            content,
            stop: if uses_tools {
                Stop::ToolUse
            } else {
                Stop::EndTurn
            },
            usage: Usage::default(),
        });
        self
    }

    fn asked(&self) -> usize {
        self.requests.borrow().len()
    }
}

impl Model for Script {
    async fn ask(&self, request: &ModelRequest) -> Result<ModelReply, ModelError> {
        self.requests.borrow_mut().push(request.clone());
        self.replies
            .borrow_mut()
            .pop_front()
            .ok_or_else(|| ModelError("the script has no reply left".into()))
    }
}

/// Files as rows in memory. Its clones share the rows.
#[derive(Clone, Default)]
struct Rows(Arc<Mutex<BTreeMap<String, Vec<u8>>>>);

impl Rows {
    fn text(&self, path: &str) -> Option<String> {
        let rows = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        rows.get(path)
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
    }
}

impl FileSystem for Rows {
    fn read(&self, path: &str) -> FsFuture<'_, Vec<u8>> {
        let found = rows::key(path).and_then(|key| {
            let rows = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            rows.get(&key).cloned().ok_or("no such file".into())
        });
        Box::pin(async move { found })
    }

    fn write(&self, path: &str, content: Vec<u8>) -> FsFuture<'_, ()> {
        let wrote = rows::key(path).map(|key| {
            let mut rows = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            rows.insert(key, content);
        });
        Box::pin(async move { wrote })
    }

    fn list(&self, path: &str) -> FsFuture<'_, Vec<String>> {
        let listed = rows::key(path).and_then(|directory| {
            let rows = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            rows::children(&directory, rows.keys().map(String::as_str))
                .ok_or("no such directory".into())
        });
        Box::pin(async move { listed })
    }
}

/// What a host holds: the storage and files that last, and the object and
/// model of this run.
struct Host {
    storage: MemoryStorage,
    files: Rows,
    capabilities: Capabilities,
    model: Script,
    object: RefCell<Object<MemoryStorage>>,
    boots: u32,
}

impl Host {
    fn new() -> Self {
        let storage = MemoryStorage::new();
        let files = Rows::default();
        Self::over(storage, files, 1)
    }

    fn over(storage: MemoryStorage, files: Rows, boots: u32) -> Self {
        let capabilities = Capabilities::new().with(Files::new(files.clone()));
        let object = Object::start(
            Name::new("a1").unwrap(),
            || format!("boot-{boots}"),
            capabilities.declare(),
            Settings {
                system: Some("Be brief.".into()),
                compact_at: Some(100),
                ..Settings::default()
            },
            storage.clone(),
        )
        .unwrap();
        Self {
            storage,
            files,
            capabilities,
            model: Script::default(),
            object: RefCell::new(object),
            boots,
        }
    }

    /// The object stops where it is, and is opened again over what it
    /// stored.
    fn reopened(self) -> Self {
        Self::over(self.storage, self.files, self.boots + 1)
    }

    fn prompt(&self, text: &str) {
        self.object.borrow_mut().prompt(None, text.into()).unwrap();
    }

    /// Does work until none is left.
    fn run(&self) {
        while now(step(&self.object, &self.capabilities, &self.model)).unwrap() {}
    }

    fn items(&self) -> Vec<Item> {
        self.object.borrow().read(0, usize::MAX).unwrap()
    }

    fn state(&self) -> State {
        self.object.borrow().state()
    }
}

fn uses(id: &str, name: &str, input: Value) -> Block {
    Block::ToolUse {
        id: id.into(),
        name: name.into(),
        input,
    }
}

fn says(text: &str) -> Block {
    Block::Text { text: text.into() }
}

fn kinds(items: &[Item]) -> Vec<&'static str> {
    items
        .iter()
        .map(|item| match item {
            Item::Prompt { .. } => "prompt",
            Item::Step { .. } => "step",
            Item::Call { .. } => "call",
            Item::Outcome { .. } => "outcome",
            Item::Attached { .. } => "attached",
            Item::Paused { .. } => "paused",
            Item::Failed { .. } => "failed",
            Item::Summary { .. } => "summary",
        })
        .collect()
}

fn outcome<'a>(items: &'a [Item], of: &str) -> &'a Outcome {
    items
        .iter()
        .find_map(|item| match item {
            Item::Outcome {
                tool_use, outcome, ..
            } if tool_use == of => Some(outcome),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{of} has no outcome"))
}

/// The text of every tool result in a request, with whether it is an error.
fn told(request: &ModelRequest) -> Vec<(String, bool)> {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::ToolResult { text, error, .. } => Some((text.clone(), *error)),
            _ => None,
        })
        .collect()
}

#[test]
fn a_prompt_is_answered_through_the_hand() {
    let host = Host::new();
    host.model
        .then(vec![uses(
            "tu_1",
            "write",
            json!({"path": "notes/haiku.txt", "content": "an old pond\n"}),
        )])
        .then(vec![says("Written.")]);

    host.prompt("Write a haiku to notes/haiku.txt.");
    assert_eq!(host.state(), State::Thinking);
    host.run();

    assert_eq!(host.state(), State::Idle);
    assert_eq!(
        kinds(&host.items()),
        ["attached", "prompt", "step", "call", "outcome", "step"]
    );
    assert_eq!(
        host.files.text("notes/haiku.txt").as_deref(),
        Some("an old pond\n")
    );
    assert_eq!(
        outcome(&host.items(), "tu_1"),
        &Outcome::Output(json!("wrote 12 bytes to notes/haiku.txt"))
    );

    // The model was offered the hand's operations as tools, with the
    // system prompt, and was told how its call ended.
    let requests = host.model.requests.borrow();
    let tools: Vec<_> = requests[0]
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert_eq!(tools, ["read", "write", "edit", "list"]);
    assert_eq!(requests[0].system.as_deref(), Some("Be brief."));
    assert_eq!(
        told(&requests[1]),
        [("wrote 12 bytes to notes/haiku.txt".to_owned(), false)]
    );

    // Everything in memory is also stored.
    assert_eq!(host.storage.stored().items, host.items());
}

#[test]
fn an_object_opened_again_is_the_same_hand_and_stores_nothing_new() {
    let host = Host::new();
    host.model.then(vec![says("Hello.")]);
    host.prompt("Say hello.");
    host.run();
    let before = host.storage.stored();

    let host = host.reopened();
    assert_eq!(host.state(), State::Idle);
    assert!(!host.object.borrow().has_work());
    assert_eq!(host.storage.stored(), before);
    assert_eq!(kinds(&host.items()), ["attached", "prompt", "step"]);
}

#[test]
fn nothing_opens_where_no_pair_has_started() {
    let storage = MemoryStorage::new();
    let opened = Object::open(Vec::new(), Settings::default(), storage.clone()).unwrap();
    assert!(opened.is_none());
    assert_eq!(storage.stored().items, []);

    let host = Host::over(storage.clone(), Rows::default(), 1);
    host.prompt("Say hello.");
    let opened = Object::open(Vec::new(), Settings::default(), storage)
        .unwrap()
        .unwrap();
    assert_eq!(opened.name().as_str(), "a1");
    assert_eq!(opened.read(0, usize::MAX).unwrap(), host.items());
}

#[test]
fn an_object_that_stops_while_the_model_is_asked_asks_again() {
    let host = Host::new();
    host.prompt("Say hello.");
    // The host takes the request, and stops before any answer.
    assert!(matches!(
        host.object.borrow_mut().take(),
        Some(Work::Ask(_))
    ));
    let stored = host.storage.stored().items.len();

    let host = host.reopened();
    assert_eq!(host.storage.stored().items.len(), stored);
    host.model.then(vec![says("Hello.")]);
    host.run();

    assert_eq!(host.model.asked(), 1);
    assert_eq!(kinds(&host.items()), ["attached", "prompt", "step"]);
    assert_eq!(host.state(), State::Idle);
}

#[test]
fn a_call_that_was_running_when_the_object_stopped_is_lost_and_never_run_again() {
    let host = Host::new();
    host.model.then(vec![uses(
        "tu_1",
        "write",
        json!({"path": "a.txt", "content": "one"}),
    )]);
    host.prompt("Write a.txt.");
    assert!(now(step(&host.object, &host.capabilities, &host.model)).unwrap());
    // The host takes the call, and stops before it reports how it ended.
    assert!(matches!(
        host.object.borrow_mut().take(),
        Some(Work::Call(call)) if call.id == 1
    ));

    let host = host.reopened();
    let items = host.items();
    assert_eq!(
        kinds(&items),
        ["attached", "prompt", "step", "call", "outcome"]
    );
    let Outcome::Error(error) = outcome(&items, "tu_1") else {
        panic!("the call should have ended as an error");
    };
    assert!(error.contains("its outcome is unknown"), "{error}");

    // The call is not offered again: the model is asked what to do next.
    host.model.then(vec![says("I will check before I retry.")]);
    host.run();
    assert_eq!(host.files.text("a.txt"), None);
    let requests = host.model.requests.borrow();
    assert_eq!(requests.len(), 1);
    assert!(matches!(&told(&requests[0])[..], [(_, true)]));
    assert_eq!(host.state(), State::Idle);
}

#[test]
fn a_prompt_that_arrives_mid_turn_joins_the_next_model_call() {
    let host = Host::new();
    host.model
        .then(vec![uses("tu_1", "list", json!({}))])
        .then(vec![says("Nothing there, and noted.")]);
    host.prompt("List the files.");
    assert!(now(step(&host.object, &host.capabilities, &host.model)).unwrap());

    // The model has asked for a call. A second prompt arrives before it runs.
    host.prompt("Also: be quick.");
    assert_eq!(host.state(), State::Working);
    host.run();

    let requests = host.model.requests.borrow();
    assert_eq!(requests.len(), 2);
    let last = requests[1].messages.last().unwrap();
    assert!(
        last.content
            .iter()
            .any(|content| matches!(content, Content::Text(text) if text == "Also: be quick.")),
        "{last:?}"
    );
    assert_eq!(host.state(), State::Idle);
}

#[test]
fn a_prompt_under_a_key_is_stored_once() {
    let host = Host::new();
    let prompt = |key: &str, text: &str| {
        host.object
            .borrow_mut()
            .prompt(Some(key.into()), text.into())
    };
    assert_eq!(prompt("k1", "Say hello."), Ok(false));
    assert_eq!(prompt("k1", "Say hello."), Ok(true));
    assert!(matches!(
        prompt("k1", "Say goodbye."),
        Err(PromptError::Conflict(_))
    ));
    assert_eq!(kinds(&host.items()), ["attached", "prompt"]);
}

#[test]
fn a_call_that_fails_is_an_error_the_model_reads() {
    let host = Host::new();
    host.model
        .then(vec![uses("tu_1", "read", json!({"path": "missing.txt"}))])
        .then(vec![says("There is no such file.")]);
    host.prompt("Read missing.txt.");
    host.run();

    let items = host.items();
    let Outcome::Error(error) = outcome(&items, "tu_1") else {
        panic!("the read should have failed");
    };
    assert_eq!(error, "missing.txt could not be read: no such file");
    assert_eq!(host.state(), State::Idle);
}

#[test]
fn a_model_that_cannot_be_asked_ends_the_turn_until_the_next_prompt() {
    let host = Host::new();
    host.prompt("Say hello.");
    host.run();
    assert_eq!(kinds(&host.items()), ["attached", "prompt", "failed"]);
    assert_eq!(host.state(), State::Idle);

    host.model.then(vec![says("Hello.")]);
    host.prompt("Try again.");
    host.run();
    assert_eq!(
        kinds(&host.items()),
        ["attached", "prompt", "failed", "prompt", "step"]
    );
}

#[test]
fn an_interrupt_ends_the_turn_whatever_the_host_was_doing() {
    // While the model is asked.
    let host = Host::new();
    host.prompt("Write a long essay.");
    let Some(Work::Ask(request)) = host.object.borrow_mut().take() else {
        panic!("the model should be asked");
    };
    host.object.borrow_mut().interrupt().unwrap();
    assert_eq!(host.state(), State::Idle);
    assert_eq!(kinds(&host.items()), ["attached", "prompt", "paused"]);
    // The answer comes anyway, and changes nothing.
    let late = ModelReply {
        content: vec![says("Once upon a time")],
        stop: Stop::EndTurn,
        usage: Usage::default(),
    };
    let mut object = host.object.borrow_mut();
    object.replied(request.seen, Ok(late)).unwrap();
    assert_eq!(object.length(), 3);
    assert!(!object.has_work());
    drop(object);

    // Before the host has asked the model at all.
    let host = Host::new();
    host.prompt("Write a long essay.");
    host.object.borrow_mut().interrupt().unwrap();
    assert!(!host.object.borrow().has_work());
    host.run();
    assert_eq!(host.model.asked(), 0);

    // While a call waits to run: it never does.
    let host = Host::new();
    host.model.then(vec![uses(
        "tu_1",
        "write",
        json!({"path": "a.txt", "content": "one"}),
    )]);
    host.prompt("Write a.txt.");
    assert!(now(step(&host.object, &host.capabilities, &host.model)).unwrap());
    host.object.borrow_mut().interrupt().unwrap();
    host.run();
    assert_eq!(host.files.text("a.txt"), None);
    assert_eq!(host.model.asked(), 1);
    assert_eq!(host.state(), State::Idle);
    let items = host.items();
    assert_eq!(
        kinds(&items),
        ["attached", "prompt", "step", "call", "paused", "outcome"]
    );
    assert_eq!(outcome(&items, "tu_1"), &Outcome::Error("cancelled".into()));

    // The next prompt starts a turn that reads how the last one ended.
    host.model.then(vec![says("Stopped, as you asked.")]);
    host.prompt("Never mind.");
    host.run();
    assert_eq!(host.state(), State::Idle);
    assert!(matches!(
        &told(&host.model.requests.borrow()[1])[..],
        [(_, true)]
    ));
}

#[test]
fn a_conversation_that_grew_too_long_carries_on_from_a_summary() {
    let host = Host::new();
    host.model.replies.borrow_mut().push_back(ModelReply {
        content: vec![says("hi")],
        stop: Stop::EndTurn,
        usage: Usage {
            input: 95,
            output: 5,
        },
    });
    host.model
        .then(vec![says("We greeted each other.")])
        .then(vec![says("Paris.")]);
    let keyed = |host: &Host, text: &str| {
        let mut object = host.object.borrow_mut();
        object.prompt(Some("k1".into()), text.into())
    };
    assert_eq!(keyed(&host, "hello"), Ok(false));
    host.run();
    host.prompt("What is the capital of France?");
    host.run();

    assert_eq!(
        kinds(&host.items()),
        ["attached", "prompt", "step", "prompt", "summary", "step"]
    );
    let requests = host.model.requests.borrow();
    assert!(requests[1].text_only);
    assert_eq!(requests[2].messages.len(), 1);
    drop(requests);

    // The summary is where the object starts when it is opened again: it
    // holds the end of its history, and its storage the whole.
    let host = host.reopened();
    assert_eq!(host.object.borrow().length(), 6);
    assert_eq!(host.items().len(), 6);
    host.model.then(vec![says("Lyon is not.")]);
    host.prompt("And Lyon?");
    host.run();
    assert_eq!(host.model.requests.borrow()[0].messages.len(), 3);

    // A key from before the summary is still the prompt it was.
    assert_eq!(keyed(&host, "hello"), Ok(true));
    assert!(matches!(
        keyed(&host, "goodbye"),
        Err(PromptError::Conflict(_))
    ));
    assert_eq!(host.items().len(), 8);
}

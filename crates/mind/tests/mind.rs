//! What a mind does, driven the way a host drives it.

use mensor_mind::{
    Block, Conflict, Content, Cut, Fork, ForkError, Forking, Item, Message, Mind, ModelReply,
    ModelRequest, Outcome, Output, Role, Settings, State, Stop, Usage, tail_from,
};
use mensor_protocol::{
    Call, CallId, CallResult, CapabilityDecl, FromMind, Hello, Name, OperationDecl, Reply, VERSION,
};
use serde_json::{Value, json};

/// A host that performs outputs by writing them down.
struct Host {
    mind: Mind,
    stored: Vec<Item>,
    sent: Vec<FromMind>,
    asked: Vec<ModelRequest>,
}

impl Host {
    fn new() -> Self {
        Self::around(Mind::new(Settings::default()), Vec::new())
    }

    fn around(mind: Mind, stored: Vec<Item>) -> Self {
        Self {
            mind,
            stored,
            sent: Vec::new(),
            asked: Vec::new(),
        }
    }

    /// Performs outputs, checking that a call is stored before it is sent.
    fn perform(&mut self, outputs: Vec<Output>) {
        for output in outputs {
            match output {
                Output::Append(item) => self.stored.push(item),
                Output::Send(message) => {
                    if let FromMind::Call(call) = &message {
                        let stored = self
                            .stored
                            .iter()
                            .any(|item| matches!(item, Item::Call { id, .. } if *id == call.id));
                        assert!(stored, "call {} was sent before it was stored", call.id);
                    }
                    self.sent.push(message);
                }
                Output::Ask(request) => self.asked.push(request),
            }
        }
        // The mind holds the end of what is stored.
        let from = usize::try_from(self.mind.starts_at()).unwrap();
        assert_eq!(
            &self.stored[from..],
            self.mind.items(),
            "the store and the mind disagree"
        );
    }

    fn attach(&mut self, hello: &Hello) {
        let outputs = self.mind.attach(hello);
        self.perform(outputs);
    }

    fn prompt(&mut self, text: &str) {
        let prompted = self.mind.prompt(None, text.into(), None).unwrap();
        self.perform(prompted.outputs);
    }

    /// Answers the request the mind last made.
    fn reply(&mut self, content: Vec<Block>) {
        self.reply_costing(content, Usage::default());
    }

    /// Answers the request the mind last made, at this cost.
    fn reply_costing(&mut self, content: Vec<Block>, usage: Usage) {
        let seen = self.asked.last().expect("the mind asked nothing").seen;
        let stop = if content
            .iter()
            .any(|block| matches!(block, Block::ToolUse { .. }))
        {
            Stop::ToolUse
        } else {
            Stop::EndTurn
        };
        let outputs = self.mind.replied(
            seen,
            ModelReply {
                content,
                stop,
                usage,
            },
        );
        self.perform(outputs);
    }

    fn interrupt(&mut self) {
        let outputs = self.mind.interrupt();
        self.perform(outputs);
    }

    fn result(&mut self, id: CallId, output: Value) {
        let outputs = self.mind.result(CallResult {
            id,
            reply: Reply::Output(output),
        });
        self.perform(outputs);
    }

    fn sent(&mut self) -> Vec<FromMind> {
        std::mem::take(&mut self.sent)
    }

    fn asked(&mut self) -> usize {
        self.asked.len()
    }

    fn last_message(&self) -> &Message {
        self.asked.last().unwrap().messages.last().unwrap()
    }
}

fn name(s: &str) -> Name {
    Name::new(s).unwrap()
}

fn hello(boot: &str) -> Hello {
    Hello {
        v: VERSION,
        name: name("a1"),
        was: None,
        boot: boot.into(),
        last_id: 0,
        running: Vec::new(),
        held: Vec::new(),
        capabilities: vec![CapabilityDecl {
            name: "commands".into(),
            operations: vec![OperationDecl {
                name: "bash".into(),
                description: "Run a command.".into(),
                input: json!({"type": "object"}),
            }],
        }],
    }
}

fn clone_of(parent: &Hello, last_id: CallId) -> Hello {
    Hello {
        name: name("b7"),
        was: Some(parent.name.clone()),
        last_id,
        ..parent.clone()
    }
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

fn call(id: CallId, command: &str) -> FromMind {
    FromMind::Call(Call {
        id,
        capability: "commands".into(),
        operation: "bash".into(),
        input: json!({"command": command}),
    })
}

fn ack(ids: &[CallId]) -> FromMind {
    FromMind::Ack { ids: ids.to_vec() }
}

fn held(id: CallId, output: Value) -> CallResult {
    CallResult {
        id,
        reply: Reply::Output(output),
    }
}

fn told(tool_use: &str, text: &str) -> Content {
    Content::ToolResult {
        tool_use: tool_use.into(),
        text: text.into(),
        error: false,
    }
}

/// The mind of a clone of `parent`'s hand, made as a host makes it: from
/// what `parent` stored, read a few items at a time and never held whole.
fn fork(parent: &Host, hello: &Hello) -> Result<Fork, ForkError> {
    let mut made = None;
    for page in [1, 2, 3, 512] {
        let mut cut = Cut::new(hello);
        for items in parent.stored.chunks(page) {
            cut.read(items);
        }
        let parting = cut.end()?;
        let (from, to) = (
            usize::try_from(parting.from).unwrap(),
            usize::try_from(parting.cut).unwrap(),
        );
        let shared = parent.stored[from..to].to_vec();
        let mut forking = Forking::new(parting, shared, Settings::default(), hello);
        for items in parent.stored[to..].chunks(page) {
            if !forking.wants() {
                break;
            }
            forking.later(items);
        }
        // However the history is paged, the fork is the same.
        let fork = forking.end();
        if let Some((inherited, outputs)) = &made {
            assert_eq!((inherited, outputs), (&fork.inherited, &fork.outputs));
        }
        made = Some((fork.inherited, fork.outputs.clone()));
        if page == 512 {
            return Ok(fork);
        }
    }
    unreachable!("the last page size returns")
}

/// What the model is told of a pause.
fn stopped(reason: &str) -> Content {
    Content::Text(format!("(The turn before this was stopped: {reason}.)"))
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

/// A host whose mind has asked its hand for one command and waits on it.
fn waiting_on_a_call() -> Host {
    let mut host = Host::new();
    host.attach(&hello("boot-1"));
    host.prompt("list the files");
    host.reply(vec![bash("tu_1", "ls")]);
    assert_eq!(host.sent(), vec![call(1, "ls")]);
    host
}

#[test]
fn a_turn_runs_from_a_prompt_to_an_answer() {
    let mut host = Host::new();
    host.attach(&hello("boot-1"));
    assert_eq!(
        host.stored,
        vec![Item::Attached {
            boot: "boot-1".into()
        }]
    );
    assert_eq!(host.mind.state(), State::Idle);

    host.prompt("list the files");
    let request = host.asked.last().unwrap();
    assert_eq!(
        request.messages,
        vec![Message {
            role: Role::User,
            content: vec![Content::Text("list the files".into())]
        }]
    );
    assert_eq!(request.tools.len(), 1);
    assert_eq!(request.tools[0].name, "bash");
    assert_eq!(host.mind.state(), State::Thinking);

    host.reply(vec![text("Looking."), bash("tu_1", "ls")]);
    assert_eq!(host.sent(), vec![call(1, "ls")]);
    assert_eq!(host.mind.state(), State::Working);

    host.result(1, json!("Cargo.toml"));
    assert_eq!(host.sent(), vec![ack(&[1])]);
    assert_eq!(
        host.last_message().content,
        vec![told("tu_1", "Cargo.toml")]
    );

    host.reply(vec![text("One file.")]);
    assert_eq!(host.mind.state(), State::Idle);
    assert_eq!(host.asked(), 2);
}

#[test]
fn the_calls_of_one_step_run_one_at_a_time_in_order() {
    let mut host = Host::new();
    host.attach(&hello("boot-1"));
    host.prompt("build and test");
    host.reply(vec![
        bash("tu_1", "cargo build"),
        bash("tu_2", "cargo test"),
    ]);
    assert_eq!(host.sent(), vec![call(1, "cargo build")]);

    host.result(1, json!("built"));
    assert_eq!(host.sent(), vec![ack(&[1]), call(2, "cargo test")]);
    assert_eq!(host.asked(), 1);

    host.result(2, json!("passed"));
    assert_eq!(host.asked(), 2);
    assert_eq!(
        host.last_message().content,
        vec![told("tu_1", "built"), told("tu_2", "passed")]
    );
}

#[test]
fn a_tool_the_hand_lacks_does_not_run() {
    let mut host = Host::new();
    host.attach(&hello("boot-1"));
    host.prompt("fly");
    host.reply(vec![Block::ToolUse {
        id: "tu_1".into(),
        name: "fly".into(),
        input: json!({}),
    }]);
    assert!(host.sent().is_empty());
    assert!(matches!(outcome(&host.stored, "tu_1"), Outcome::NotRun(why) if why.contains("fly")));
    assert_eq!(host.asked(), 2);
}

#[test]
fn a_prompt_mid_turn_joins_the_next_model_call() {
    let mut host = waiting_on_a_call();
    host.prompt("and count them");
    assert_eq!(host.asked(), 1, "the round is still open");

    host.result(1, json!("Cargo.toml"));
    assert_eq!(
        host.last_message().content,
        vec![
            told("tu_1", "Cargo.toml"),
            Content::Text("and count them".into())
        ]
    );
}

#[test]
fn a_prompt_that_arrives_while_the_model_is_asked_gets_a_step_of_its_own() {
    let mut host = Host::new();
    host.attach(&hello("boot-1"));
    host.prompt("first");
    host.prompt("second");
    assert_eq!(host.asked(), 1, "one request at a time");

    host.reply(vec![text("answer to first")]);
    assert_eq!(host.asked(), 2, "the late prompt is still owed an answer");
    let roles: Vec<_> = host.asked[1]
        .messages
        .iter()
        .map(|message| message.role)
        .collect();
    assert_eq!(roles, [Role::User, Role::Assistant, Role::User]);
    assert_eq!(
        host.last_message().content,
        vec![Content::Text("second".into())]
    );

    host.reply(vec![text("answer to second")]);
    assert_eq!(host.mind.state(), State::Idle);
}

#[test]
fn a_prompt_under_a_key_is_stored_once() {
    let mut host = Host::new();
    host.attach(&hello("boot-1"));
    let keyed = |mind: &mut Mind, text: &str| mind.prompt(Some("k1".into()), text.into(), None);
    let first = keyed(&mut host.mind, "hello").unwrap();
    assert!(!first.existing);
    host.perform(first.outputs);

    let again = keyed(&mut host.mind, "hello").unwrap();
    assert!(again.existing && again.outputs.is_empty());
    assert_eq!(
        keyed(&mut host.mind, "other"),
        Err(Conflict { key: "k1".into() })
    );
    assert_eq!(
        host.stored
            .iter()
            .filter(|item| matches!(item, Item::Prompt { .. }))
            .count(),
        1
    );
}

#[test]
fn a_mind_without_a_hand_only_listens() {
    let mut host = Host::new();
    host.prompt("anyone there?");
    assert_eq!(host.asked(), 0);
    assert_eq!(host.mind.state(), State::Thinking);

    host.attach(&hello("boot-1"));
    assert_eq!(host.asked(), 1);
}

#[test]
fn a_dropped_connection_stops_new_calls_until_the_hand_returns() {
    let mut host = Host::new();
    host.attach(&hello("boot-1"));
    host.prompt("list the files");
    host.mind.detach();
    host.reply(vec![bash("tu_1", "ls")]);
    assert!(
        host.sent().is_empty(),
        "nothing is sent to a hand that is gone"
    );
    assert!(
        !host
            .stored
            .iter()
            .any(|item| matches!(item, Item::Call { .. }))
    );

    host.attach(&hello("boot-1"));
    assert_eq!(host.sent(), vec![call(1, "ls")]);
}

#[test]
fn a_reconnect_settles_each_call_by_what_the_hand_says() {
    // The hand is still running it: wait.
    let mut host = waiting_on_a_call();
    host.mind.detach();
    host.attach(&Hello {
        last_id: 1,
        running: vec![1],
        ..hello("boot-1")
    });
    assert!(host.sent().is_empty());
    assert_eq!(host.asked(), 1);
    host.result(1, json!("late"));
    assert_eq!(host.last_message().content, vec![told("tu_1", "late")]);

    // The hand holds its result: take it, then ack.
    let mut host = waiting_on_a_call();
    host.mind.detach();
    host.attach(&Hello {
        last_id: 1,
        held: vec![held(1, json!("held"))],
        ..hello("boot-1")
    });
    assert_eq!(host.sent(), vec![ack(&[1])]);
    assert_eq!(host.last_message().content, vec![told("tu_1", "held")]);

    // The hand never read it: send it again under the same id.
    let mut host = waiting_on_a_call();
    host.mind.detach();
    host.attach(&hello("boot-1"));
    assert_eq!(host.sent(), vec![call(1, "ls")]);
    assert_eq!(host.asked(), 1);

    // The hand read it and holds nothing: unknown, and never sent again.
    let mut host = waiting_on_a_call();
    host.mind.detach();
    host.attach(&Hello {
        last_id: 1,
        ..hello("boot-1")
    });
    assert!(host.sent().is_empty());
    assert!(
        matches!(outcome(&host.stored, "tu_1"), Outcome::Unknown(why) if why.contains("holds no result"))
    );
    assert_eq!(host.asked(), 2);

    // The hand restarted: unknown, and the new boot is recorded.
    let mut host = waiting_on_a_call();
    host.mind.detach();
    host.attach(&hello("boot-2"));
    assert!(host.sent().is_empty());
    assert!(
        matches!(outcome(&host.stored, "tu_1"), Outcome::Unknown(why) if why.contains("restarted"))
    );
    assert_eq!(
        host.stored.last(),
        Some(&Item::Attached {
            boot: "boot-2".into()
        })
    );
    let Content::ToolResult {
        text, error: true, ..
    } = &host.last_message().content[0]
    else {
        panic!("the model is not told");
    };
    assert!(text.contains("Check the current state"));
}

#[test]
fn a_mind_loaded_from_its_history_carries_on() {
    let before = waiting_on_a_call();
    let mut host = Host::around(
        Mind::load(0, before.stored.clone(), Settings::default()),
        before.stored,
    );
    assert_eq!(host.mind.state(), State::Working);
    assert!(!host.mind.is_attached());

    host.attach(&Hello {
        last_id: 1,
        running: vec![1],
        ..hello("boot-1")
    });
    assert!(host.sent().is_empty() && host.asked() == 0);
    host.result(1, json!("Cargo.toml"));
    assert_eq!(host.asked(), 1);
    assert_eq!(host.asked[0].messages.len(), 3);
}

#[test]
fn a_mind_loaded_while_its_model_was_asked_asks_again() {
    let mut before = Host::new();
    before.attach(&hello("boot-1"));
    before.prompt("hello");
    assert_eq!(before.asked(), 1);

    let mut host = Host::around(
        Mind::load(0, before.stored.clone(), Settings::default()),
        before.stored,
    );
    host.attach(&hello("boot-1"));
    assert_eq!(host.asked(), 1);
}

#[test]
fn a_second_result_for_a_call_is_only_acked() {
    let mut host = waiting_on_a_call();
    host.result(1, json!("first"));
    host.sent();
    let stored = host.stored.len();
    host.result(1, json!("second"));
    host.result(9, json!("never asked"));
    assert_eq!(host.sent(), vec![ack(&[1]), ack(&[9])]);
    assert_eq!(host.stored.len(), stored);
    assert_eq!(
        outcome(&host.stored, "tu_1"),
        &Outcome::Output(json!("first"))
    );
}

#[test]
fn a_model_that_fails_stops_the_turn_until_the_next_prompt() {
    let mut host = Host::new();
    host.attach(&hello("boot-1"));
    host.prompt("hello");
    let seen = host.asked.last().unwrap().seen;
    let outputs = host.mind.failed(seen, "the model is down");
    host.perform(outputs);
    assert_eq!(
        host.stored.last(),
        Some(&Item::Failed {
            error: "the model is down".into()
        })
    );
    assert_eq!(host.mind.state(), State::Idle);

    host.prompt("are you back?");
    assert_eq!(host.asked(), 2);
    assert_eq!(
        host.last_message().content,
        vec![
            Content::Text("hello".into()),
            Content::Text("are you back?".into())
        ]
    );
}

#[test]
fn a_reply_nobody_asked_for_is_dropped() {
    let mut host = Host::new();
    host.attach(&hello("boot-1"));
    let reply = ModelReply {
        content: vec![text("?")],
        stop: Stop::EndTurn,
        usage: Usage::default(),
    };
    assert!(host.mind.replied(0, reply).is_empty());
    assert!(host.mind.failed(0, "?").is_empty());
}

#[test]
fn a_clone_taken_mid_call_gets_a_mind_that_waits_for_its_own_result() {
    let mut parent = waiting_on_a_call();
    let clone = Hello {
        running: vec![1],
        ..clone_of(&hello("boot-1"), 1)
    };
    let fork = fork(&parent, &clone).unwrap();

    // It shares everything up to the call the clone had read.
    assert_eq!(fork.inherited, 4);
    let mut child = Host::around(fork.mind, parent.stored[..4].to_vec());
    child.perform(fork.outputs);
    assert_eq!(
        child.stored[4],
        Item::Paused {
            reason: "forked from a1".into()
        }
    );
    assert!(child.sent().is_empty());
    assert_eq!(child.mind.state(), State::Working);

    // Each hand finishes the command on its own, and each mind takes its own.
    parent.result(1, json!("parent's files"));
    child.result(1, json!("clone's files"));
    assert_eq!(
        outcome(&parent.stored, "tu_1"),
        &Outcome::Output(json!("parent's files"))
    );
    assert_eq!(
        outcome(&child.stored, "tu_1"),
        &Outcome::Output(json!("clone's files"))
    );

    // The parent carries on. The new mind is idle until it is prompted.
    assert_eq!(parent.asked(), 2);
    assert_eq!(child.asked(), 0);
    assert_eq!(child.mind.state(), State::Idle);

    child.prompt("now delete them");
    assert_eq!(child.asked(), 1);
    assert_eq!(
        child.last_message().content,
        vec![
            told("tu_1", "clone's files"),
            stopped("forked from a1"),
            Content::Text("now delete them".into())
        ]
    );
}

#[test]
fn a_clone_that_had_finished_its_last_call_shares_what_followed_it() {
    let mut parent = waiting_on_a_call();
    parent.result(1, json!("Cargo.toml"));
    parent.reply(vec![text("One file.")]);

    // The clone was copied after call 1 was acked: it has read it, and
    // neither runs it nor holds its result. Nothing has touched the hand
    // since, so the new mind shares the answer that followed.
    let fork = fork(&parent, &clone_of(&hello("boot-1"), 1)).unwrap();
    assert_eq!(fork.inherited, 6);
    let mut child = Host::around(fork.mind, parent.stored.clone());
    child.perform(fork.outputs);

    assert_eq!(
        outcome(&child.stored, "tu_1"),
        &Outcome::Output(json!("Cargo.toml"))
    );
    assert!(matches!(child.stored.last(), Some(Item::Paused { .. })));
    assert_eq!(child.stored.len(), 7);
    assert_eq!(child.mind.state(), State::Idle);
    assert!(child.sent().is_empty() && child.asked() == 0);

    child.prompt("and how big is it?");
    let roles: Vec<_> = child.asked[0]
        .messages
        .iter()
        .map(|message| message.role)
        .collect();
    assert_eq!(
        roles,
        [
            Role::User,
            Role::Assistant,
            Role::User,
            Role::Assistant,
            Role::User
        ]
    );
}

#[test]
fn a_clone_shares_nothing_past_the_next_call_it_never_read() {
    let mut parent = waiting_on_a_call();
    parent.result(1, json!("Cargo.toml"));
    parent.reply(vec![bash("tu_2", "wc -l Cargo.toml")]);
    parent.result(2, json!("12"));

    // Copied after call 1 was acked, before the hand read call 2.
    let fork = fork(&parent, &clone_of(&hello("boot-1"), 1)).unwrap();
    assert_eq!(fork.inherited, 6, "up to the step that asked for call 2");
    let mut child = Host::around(fork.mind, parent.stored[..6].to_vec());
    child.perform(fork.outputs);
    assert!(matches!(outcome(&child.stored, "tu_2"), Outcome::NotRun(_)));
    assert!(child.sent().is_empty());
    assert_eq!(child.mind.state(), State::Idle);
}

#[test]
fn a_clone_that_holds_a_result_gives_it_to_its_own_mind() {
    let parent = waiting_on_a_call();
    let clone = Hello {
        held: vec![held(1, json!("held by the clone"))],
        ..clone_of(&hello("boot-1"), 1)
    };
    let fork = fork(&parent, &clone).unwrap();
    let mut child = Host::around(fork.mind, parent.stored[..4].to_vec());
    child.perform(fork.outputs);

    assert_eq!(
        outcome(&child.stored, "tu_1"),
        &Outcome::Output(json!("held by the clone"))
    );
    assert_eq!(child.sent(), vec![ack(&[1])]);
    assert_eq!(child.mind.state(), State::Idle);
}

#[test]
fn tool_uses_that_were_not_yet_calls_do_not_run_in_a_new_mind() {
    let mut parent = Host::new();
    parent.attach(&hello("boot-1"));
    parent.prompt("build and test");
    parent.reply(vec![
        bash("tu_1", "cargo build"),
        bash("tu_2", "cargo test"),
    ]);

    let clone = Hello {
        running: vec![1],
        ..clone_of(&hello("boot-1"), 1)
    };
    let fork = fork(&parent, &clone).unwrap();
    let mut child = Host::around(fork.mind, parent.stored[..4].to_vec());
    child.perform(fork.outputs);
    assert!(
        matches!(outcome(&child.stored, "tu_2"), Outcome::NotRun(why) if why.contains("forked"))
    );

    child.result(1, json!("built"));
    assert_eq!(
        child.sent(),
        vec![ack(&[1])],
        "the second command is never sent"
    );
    assert_eq!(child.mind.state(), State::Idle);

    child.prompt("carry on");
    let content = &child.last_message().content;
    assert_eq!(content[0], told("tu_1", "built"));
    assert!(
        matches!(&content[1], Content::ToolResult { tool_use, error: true, .. } if tool_use == "tu_2")
    );
    assert_eq!(content[2], stopped("forked from a1"));
    assert_eq!(content[3], Content::Text("carry on".into()));
}

#[test]
fn a_clone_that_has_read_nothing_shares_the_talk_before_the_first_call() {
    let mut parent = Host::new();
    parent.attach(&hello("boot-1"));
    parent.prompt("hello");
    parent.reply(vec![text("Hello.")]);
    parent.prompt("list the files");
    parent.reply(vec![bash("tu_1", "ls")]);

    // Copied before the hand read call 1.
    let fork = fork(&parent, &clone_of(&hello("boot-1"), 0)).unwrap();
    assert_eq!(fork.inherited, 5, "everything before the call");
    let mut child = Host::around(fork.mind, parent.stored[..5].to_vec());
    child.perform(fork.outputs);
    assert!(matches!(outcome(&child.stored, "tu_1"), Outcome::NotRun(_)));
    assert_eq!(child.mind.state(), State::Idle);
    assert!(child.sent().is_empty());
}

#[test]
fn a_clone_made_without_its_memory_shares_everything_and_knows_nothing() {
    let parent = waiting_on_a_call();
    let fork = fork(&parent, &clone_of(&hello("boot-9"), 0)).unwrap();
    assert_eq!(fork.inherited, 4);
    let mut child = Host::around(fork.mind, parent.stored.clone());
    child.perform(fork.outputs);

    assert!(
        matches!(&child.stored[4], Item::Paused { reason } if reason.contains("without its memory"))
    );
    assert!(
        matches!(outcome(&child.stored, "tu_1"), Outcome::Unknown(why) if why.contains("restarted"))
    );
    assert_eq!(
        child.stored.last(),
        Some(&Item::Attached {
            boot: "boot-9".into()
        })
    );
    assert!(child.sent().is_empty());
    assert_eq!(child.mind.state(), State::Idle);
}

#[test]
fn a_new_mind_numbers_its_calls_on_from_its_parents() {
    let mut parent = waiting_on_a_call();
    parent.result(1, json!("Cargo.toml"));
    let fork = fork(&parent, &clone_of(&hello("boot-1"), 1)).unwrap();
    let mut child = Host::around(fork.mind, parent.stored.clone());
    child.perform(fork.outputs);

    child.prompt("show it");
    child.reply(vec![bash("tu_2", "cat Cargo.toml")]);
    assert_eq!(child.sent(), vec![call(2, "cat Cargo.toml")]);

    parent.reply(vec![bash("tu_3", "wc -l Cargo.toml")]);
    assert_eq!(parent.sent(), vec![ack(&[1]), call(2, "wc -l Cargo.toml")]);
}

#[test]
fn a_hand_that_read_a_call_this_mind_never_made_is_no_clone_of_it() {
    let parent = waiting_on_a_call();
    let stranger = clone_of(&hello("boot-1"), 99);
    assert_eq!(
        fork(&parent, &stranger).unwrap_err(),
        ForkError::UnknownCall(99)
    );
}

#[test]
fn a_tool_use_in_a_reply_that_was_cut_off_does_not_run() {
    let mut host = Host::new();
    host.attach(&hello("boot-1"));
    host.prompt("write a long file");
    let reply = ModelReply {
        content: vec![bash("tu_1", "cat > file <<EOF")],
        stop: Stop::MaxTokens,
        usage: Usage::default(),
    };
    let outputs = host.mind.replied(host.asked[0].seen, reply);
    host.perform(outputs);

    assert!(host.sent().is_empty());
    assert!(
        matches!(outcome(&host.stored, "tu_1"), Outcome::NotRun(why) if why.contains("stopped before"))
    );
    assert_eq!(host.asked(), 2, "the model is told, and asked again");
}

#[test]
fn a_block_the_mind_does_not_read_goes_back_to_the_model_as_it_came() {
    let reasoning = json!({"type": "thinking", "thinking": "", "signature": "sig"});
    let mut host = Host::new();
    host.attach(&hello("boot-1"));
    host.prompt("list the files");
    host.reply(vec![
        Block::Other {
            value: reasoning.clone(),
        },
        bash("tu_1", "ls"),
    ]);
    host.result(1, json!("Cargo.toml"));

    let assistant = &host.asked[1].messages[1];
    assert_eq!(assistant.role, Role::Assistant);
    assert_eq!(assistant.content[0], Content::Other(reasoning));
    assert!(matches!(&assistant.content[1], Content::ToolUse { id, .. } if id == "tu_1"));
}

#[test]
fn an_interrupt_drops_the_reply_the_model_was_writing() {
    let mut host = Host::new();
    host.attach(&hello("boot-1"));
    host.prompt("write a long essay");
    assert_eq!(host.mind.state(), State::Thinking);

    host.interrupt();
    assert_eq!(
        host.stored.last(),
        Some(&Item::Paused {
            reason: "interrupted".into()
        })
    );
    assert_eq!(host.mind.state(), State::Idle);

    // The reply arrives anyway, and is not stored.
    host.reply(vec![text("Once upon a time")]);
    assert_eq!(host.stored.len(), 3);

    host.prompt("a short one, then");
    assert_eq!(host.asked(), 2);
    assert_eq!(
        host.last_message().content,
        vec![
            Content::Text("write a long essay".into()),
            stopped("interrupted"),
            Content::Text("a short one, then".into())
        ]
    );
}

#[test]
fn an_interrupt_cancels_the_running_call_and_asks_no_model_after_it() {
    let mut host = waiting_on_a_call();
    host.interrupt();
    assert_eq!(host.sent(), vec![FromMind::Cancel { id: 1 }]);
    // The hand's answer to the cancel is still owed.
    assert_eq!(host.mind.state(), State::Working);

    let outputs = host.mind.result(CallResult {
        id: 1,
        reply: Reply::Error("cancelled".into()),
    });
    host.perform(outputs);
    assert_eq!(host.sent(), vec![ack(&[1])]);
    assert_eq!(host.mind.state(), State::Idle);
    assert_eq!(host.asked(), 1);

    host.prompt("try something else");
    assert_eq!(host.asked(), 2);
    assert_eq!(
        host.last_message().content,
        vec![
            Content::ToolResult {
                tool_use: "tu_1".into(),
                text: "cancelled".into(),
                error: true
            },
            stopped("interrupted"),
            Content::Text("try something else".into())
        ]
    );
}

#[test]
fn an_interrupt_of_a_mind_whose_hand_is_away_runs_none_of_what_was_asked() {
    let mut host = Host::new();
    host.attach(&hello("boot-1"));
    host.prompt("list the files");
    host.mind.detach();
    host.reply(vec![bash("tu_1", "ls")]);
    assert!(host.sent().is_empty());

    host.interrupt();
    assert!(host.sent().is_empty());
    assert!(matches!(
        outcome(&host.stored, "tu_1"),
        Outcome::NotRun(why) if why.contains("interrupted")
    ));
    host.attach(&hello("boot-1"));
    assert!(host.sent().is_empty());
    assert_eq!(host.mind.state(), State::Idle);
}

#[test]
fn a_call_interrupted_while_the_hand_was_away_is_cancelled_when_it_is_back() {
    let mut host = waiting_on_a_call();
    host.mind.detach();
    host.interrupt();
    host.sent();

    // The hand is back, and still runs the call.
    host.attach(&Hello {
        last_id: 1,
        running: vec![1],
        ..hello("boot-1")
    });
    assert_eq!(host.sent(), vec![FromMind::Cancel { id: 1 }]);
    assert_eq!(host.mind.state(), State::Working);

    let outputs = host.mind.result(CallResult {
        id: 1,
        reply: Reply::Error("cancelled".into()),
    });
    host.perform(outputs);
    assert_eq!(host.mind.state(), State::Idle);
    assert_eq!(host.asked(), 1);
}

#[test]
fn a_call_the_hand_never_read_does_not_run_once_its_turn_is_interrupted() {
    let mut host = waiting_on_a_call();
    host.mind.detach();
    host.interrupt();
    host.sent();

    // The call was lost on its way: the hand has read nothing.
    host.attach(&hello("boot-1"));
    assert!(host.sent().is_empty());
    assert!(matches!(
        outcome(&host.stored, "tu_1"),
        Outcome::NotRun(why) if why.contains("interrupted")
    ));
    assert_eq!(host.mind.state(), State::Idle);
}

#[test]
fn an_interrupt_of_an_idle_mind_does_nothing() {
    let mut host = Host::new();
    host.attach(&hello("boot-1"));
    host.prompt("hello");
    host.reply(vec![text("hi")]);
    let stored = host.stored.len();
    host.interrupt();
    assert_eq!(host.stored.len(), stored);
}

/// A host whose mind sums its conversation up past 100 tokens.
fn compacting() -> Host {
    let mut host = Host::around(Mind::new(compact_at_100()), Vec::new());
    host.attach(&hello("boot-1"));
    host
}

fn compact_at_100() -> Settings {
    Settings {
        compact_at: Some(100),
        ..Settings::default()
    }
}

fn costing(input: u64, output: u64) -> Usage {
    Usage { input, output }
}

fn summary_of(text: &str) -> Content {
    Content::Text(format!(
        "This conversation grew too long to keep, and was replaced by this summary of it:\n\n{text}"
    ))
}

#[test]
fn a_conversation_that_grew_too_long_is_summed_up_mid_turn() {
    let mut host = compacting();
    host.prompt("list the files");
    host.reply_costing(vec![bash("tu_1", "ls")], costing(90, 20));
    host.result(1, json!("a.txt"));

    // In place of the next step, the model is asked for a summary of all
    // it has seen: it is shown the tools, and may use none.
    let asked = host.asked.last().unwrap().clone();
    assert!(asked.text_only);
    assert_eq!(asked.tools.len(), 1);
    assert_eq!(asked.messages.len(), 3);
    assert_eq!(asked.messages[2].content[0], told("tu_1", "a.txt"));
    assert!(matches!(
        &asked.messages[2].content[1],
        Content::Text(ask) if ask.contains("Summarise")
    ));
    assert_eq!(host.mind.state(), State::Thinking);

    host.reply_costing(vec![text("Listed the files: a.txt.")], costing(120, 10));
    assert_eq!(
        host.stored.last(),
        Some(&Item::Summary {
            text: "Listed the files: a.txt.".into(),
            usage: costing(120, 10),
            // The step it follows was asked for when the history held two
            // items, and made the hand's first call.
            from: 2,
            boot: Some("boot-1".into()),
            last_call: 1,
        })
    );
    // The mind has let go of what the summary replaced.
    assert_eq!(host.mind.starts_at(), 2);
    assert_eq!(host.mind.length(), 6);

    // The turn carries on from the summary alone.
    let asked = host.asked.last().unwrap().clone();
    assert!(!asked.text_only);
    assert_eq!(
        asked.messages,
        vec![Message {
            role: Role::User,
            content: vec![summary_of("Listed the files: a.txt.")]
        }]
    );

    host.reply_costing(vec![text("There is one file.")], costing(30, 5));
    host.prompt("thanks");
    let asked = host.asked.last().unwrap();
    assert!(!asked.text_only);
    assert_eq!(asked.messages.len(), 3);
    assert_eq!(
        asked.messages[2].content,
        vec![Content::Text("thanks".into())]
    );
}

#[test]
fn results_too_long_for_the_next_request_are_summed_up_before_it() {
    let mut host = compacting();
    host.prompt("show me the log");
    // The step itself is well within the limit: its result is not.
    host.reply_costing(vec![bash("tu_1", "cat log")], costing(40, 10));
    host.result(1, json!("x".repeat(300)));
    assert!(host.asked.last().unwrap().text_only);

    let mut host = compacting();
    host.prompt("show me the log");
    host.reply_costing(vec![bash("tu_1", "cat log")], costing(40, 10));
    host.result(1, json!("x".repeat(30)));
    assert!(!host.asked.last().unwrap().text_only);
}

#[test]
fn a_model_that_does_not_say_what_it_read_is_counted_for() {
    let mut host = compacting();
    host.prompt("hello");
    // No usage comes with the step. A short conversation is still short.
    host.reply(vec![text("hi")]);
    host.prompt("tell me more");
    assert!(!host.asked.last().unwrap().text_only);

    // Two hundred bytes of it are counted as the hundred tokens allowed.
    host.reply(vec![text(&"more ".repeat(40))]);
    host.prompt("and more");
    assert!(host.asked.last().unwrap().text_only);
}

#[test]
fn a_steps_results_are_shown_within_what_one_step_may_add() {
    let settings = Settings {
        max_results: 40,
        ..Settings::default()
    };
    let mut host = Host::around(Mind::new(settings), Vec::new());
    host.attach(&hello("boot-1"));
    host.prompt("show me both");
    host.reply(vec![bash("tu_1", "cat a"), bash("tu_2", "cat b")]);
    host.result(1, json!("0123456789abcdefghijKLMNOPQRST"));
    host.result(2, json!("short"));

    // Each of the two has half of the 40 bytes. The history has it all.
    assert_eq!(
        host.last_message().content,
        vec![
            told(
                "tu_1",
                "0123456789\n... 10 bytes omitted: this result was too long to show whole ...\nKLMNOPQRST"
            ),
            told("tu_2", "short"),
        ]
    );
    assert_eq!(
        outcome(&host.stored, "tu_1"),
        &Outcome::Output(json!("0123456789abcdefghijKLMNOPQRST"))
    );
}

#[test]
fn a_prompt_no_step_has_seen_follows_the_summary_as_it_was_said() {
    let mut host = compacting();
    host.prompt("hello");
    host.reply_costing(vec![text("hi")], costing(95, 5));
    assert_eq!(host.mind.state(), State::Idle);

    host.prompt("what is the capital of France?");
    let asked = host.asked.last().unwrap().clone();
    assert!(asked.text_only);
    let shown = format!("{:?}", asked.messages);
    assert!(!shown.contains("France"), "{shown}");

    host.reply(vec![text("We greeted each other.")]);
    assert_eq!(
        host.last_message().content,
        vec![
            summary_of("We greeted each other."),
            Content::Text("what is the capital of France?".into())
        ]
    );
}

#[test]
fn a_summary_with_nothing_in_it_fails_the_turn_and_is_asked_for_again() {
    let mut host = compacting();
    host.prompt("hello");
    host.reply_costing(vec![text("hi")], costing(95, 5));
    host.prompt("again");
    host.reply(vec![]);
    assert!(matches!(host.stored.last(), Some(Item::Failed { .. })));
    assert_eq!(host.mind.state(), State::Idle);

    host.prompt("once more");
    assert!(host.asked.last().unwrap().text_only);
}

#[test]
fn a_mind_loaded_from_a_history_with_a_summary_starts_from_it() {
    let mut host = compacting();
    host.prompt("hello");
    host.reply_costing(vec![text("hi")], costing(95, 5));
    host.prompt("again");
    host.reply(vec![text("We greeted each other.")]);

    let mind = Mind::load(0, host.stored.clone(), compact_at_100());
    let mut host = Host::around(mind, host.stored);
    host.attach(&hello("boot-1"));
    assert_eq!(
        host.last_message().content,
        vec![
            summary_of("We greeted each other."),
            Content::Text("again".into())
        ]
    );
}

/// A host whose mind has summed up a turn that ran one command, with the
/// model asked for the step that follows the summary.
fn summed_up() -> Host {
    let mut host = compacting();
    host.prompt("list the files");
    host.reply_costing(vec![bash("tu_1", "ls")], costing(90, 20));
    host.result(1, json!("a.txt"));
    host.reply(vec![text("Listed the files: a.txt.")]);
    host
}

#[test]
fn a_mind_given_only_the_end_of_its_history_carries_on_as_one_given_it_all() {
    let before = summed_up();
    let from = tail_from(&before.stored);
    assert_eq!(from, 2);
    let tail = before.stored[2..].to_vec();
    let mut host = Host::around(Mind::load(from, tail, compact_at_100()), before.stored);
    assert_eq!((host.mind.starts_at(), host.mind.length()), (2, 6));
    assert_eq!(host.mind.state(), State::Thinking);

    // The summary says which hand the history knew, so the same one is
    // not recorded again, and which call was the last, so the next is new.
    host.attach(&Hello {
        last_id: 1,
        ..hello("boot-1")
    });
    assert_eq!(host.stored.len(), 6);
    assert_eq!(host.asked.last().unwrap().seen, 6);
    assert_eq!(
        host.last_message().content,
        vec![summary_of("Listed the files: a.txt.")]
    );
    host.reply(vec![bash("tu_2", "ls -a")]);
    assert_eq!(host.sent().last(), Some(&call(2, "ls -a")));
    host.result(2, json!(". .. a.txt"));
    host.reply(vec![text("One file.")]);
    assert_eq!(host.mind.state(), State::Idle);

    // A prompt that came while the step was asked for is one it has not
    // seen, wherever the mind's items start.
    host.prompt("and hidden ones?");
    assert_eq!(
        host.last_message().content,
        vec![Content::Text("and hidden ones?".into())]
    );
}

#[test]
fn what_a_mind_is_doing_is_told_from_the_latest_of_its_history_alone() {
    let mut idle = Host::new();
    idle.attach(&hello("boot-1"));
    idle.prompt("hello");
    idle.reply(vec![text("hi")]);
    let mut prompted = Host::new();
    prompted.prompt("anyone there?");
    let mut late = summed_up();
    late.reply(vec![text("One file.")]);
    late.prompt("and hidden ones?");
    let mut stopped = waiting_on_a_call();
    stopped.interrupt();

    let hosts = [
        idle,
        prompted,
        late,
        stopped,
        waiting_on_a_call(),
        summed_up(),
    ];
    let states: Vec<State> = hosts.iter().map(|host| host.mind.state()).collect();
    assert_eq!(
        states,
        [
            State::Idle,
            State::Thinking,
            State::Thinking,
            State::Working,
            State::Working,
            State::Thinking
        ]
    );
    for host in hosts {
        // What a store gives as the latest of a history: the items from
        // the index its last step had seen.
        let seen = host.stored.iter().rev().find_map(|item| match item {
            Item::Step { seen, .. } => Some(*seen),
            _ => None,
        });
        let from = seen.unwrap_or(0);
        let latest = host.stored[usize::try_from(from).unwrap()..].to_vec();
        assert_eq!(
            Mind::status(from, latest),
            (host.mind.state(), host.mind.length())
        );
    }
}

/// Where a clone parts from a history that is held whole: the rule a
/// [`Cut`] must find without holding it.
fn parting_of(items: &[Item], hello: &Hello) -> Option<usize> {
    let booted = items
        .iter()
        .position(|item| matches!(item, Item::Attached { boot } if *boot == hello.boot));
    let Some(booted) = booted else {
        return Some(items.len());
    };
    let read = if hello.last_id == 0 {
        booted
    } else {
        items
            .iter()
            .position(|item| matches!(item, Item::Call { id, .. } if *id == hello.last_id))?
    };
    let with_clone = hello.running.contains(&hello.last_id)
        || hello.held.iter().any(|held| held.id == hello.last_id);
    if with_clone {
        return Some(read + 1);
    }
    let next = items[read + 1..]
        .iter()
        .position(|item| matches!(item, Item::Call { .. } | Item::Attached { .. }));
    Some(next.map_or(items.len(), |after| read + 1 + after))
}

#[test]
fn a_parting_is_found_a_page_at_a_time_as_it_is_in_a_history_held_whole() {
    // Numbers that look random and are the same every run.
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let mut draw = move |below: u64| {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 33) % below
    };
    let mut partings = 0;
    for _ in 0..4000 {
        // A history of every kind of item, in an order no mind would write
        // but the rule must still read.
        let mut items = Vec::new();
        let mut calls = 0;
        for _ in 0..draw(24) {
            let at = u64::try_from(items.len()).unwrap();
            items.push(match draw(7) {
                0 => Item::Attached {
                    boot: format!("boot-{}", draw(3)),
                },
                1 | 2 => {
                    calls += 1;
                    Item::Call {
                        id: calls,
                        tool_use: format!("tu_{calls}"),
                        capability: "commands".into(),
                        operation: "bash".into(),
                        input: json!({}),
                    }
                }
                3 => Item::Outcome {
                    tool_use: format!("tu_{calls}"),
                    call: Some(calls),
                    outcome: Outcome::Output(json!("done")),
                },
                4 => Item::Summary {
                    text: "So far.".into(),
                    usage: Usage::default(),
                    from: draw(at + 1),
                    boot: None,
                    last_call: calls,
                },
                5 => Item::Step {
                    seen: at,
                    content: Vec::new(),
                    stop: Stop::EndTurn,
                    usage: Usage::default(),
                },
                _ => Item::Prompt {
                    key: None,
                    text: "go".into(),
                },
            });
        }
        let last_id = draw(calls + 2);
        let clone = Hello {
            boot: format!("boot-{}", draw(4)),
            last_id,
            running: if draw(4) == 0 {
                vec![last_id]
            } else {
                Vec::new()
            },
            held: if draw(4) == 0 {
                vec![held(last_id, json!("kept"))]
            } else {
                Vec::new()
            },
            ..hello("boot-1")
        };

        let whole = parting_of(&items, &clone);
        for page in [1, 2, 5, 64] {
            let mut cut = Cut::new(&clone);
            for read in items.chunks(page) {
                cut.read(read);
            }
            let found = cut.end().ok();
            assert_eq!(
                found.map(|parting| usize::try_from(parting.cut).unwrap()),
                whole,
                "{clone:?} of {items:?}"
            );
            if let (Some(parting), Some(whole)) = (found, whole) {
                // The clone's mind starts where its last summary says.
                assert_eq!(parting.from, tail_from(&items[..whole]));
                partings += 1;
            }
        }
    }
    assert!(partings > 4000, "only {partings} partings were compared");
}

#[test]
fn a_key_from_before_the_items_a_mind_holds_is_one_its_host_looks_up() {
    let mut host = summed_up();
    let found = Some("hello");
    let again = host
        .mind
        .prompt(Some("k1".into()), "hello".into(), found)
        .unwrap();
    assert!(again.existing && again.outputs.is_empty());
    assert_eq!(
        host.mind.prompt(Some("k1".into()), "other".into(), found),
        Err(Conflict { key: "k1".into() })
    );
}

#[test]
fn a_fork_of_a_summed_up_history_holds_only_its_end() {
    let parent = summed_up();
    let fork = fork(&parent, &clone_of(&hello("boot-1"), 1)).unwrap();
    // The clone had finished the only call, so it shares everything.
    assert_eq!(fork.inherited, 6);
    assert_eq!(fork.mind.starts_at(), 2);
    let own: Vec<Item> = fork
        .outputs
        .iter()
        .filter_map(|output| match output {
            Output::Append(item) => Some(item.clone()),
            _ => None,
        })
        .collect();
    assert!(matches!(own.as_slice(), [Item::Paused { .. }]));

    let mut stored = parent.stored.clone();
    stored.extend(own);
    let mut child = Host::around(fork.mind, stored);
    child.prompt("and hidden ones?");
    assert_eq!(
        child.last_message().content,
        vec![
            summary_of("Listed the files: a.txt."),
            stopped("forked from a1"),
            Content::Text("and hidden ones?".into())
        ]
    );
}

#[test]
fn a_tool_use_id_a_model_uses_twice_is_answered_in_its_own_round() {
    let mut host = Host::new();
    host.attach(&hello("boot-1"));
    host.prompt("list the files, twice");
    host.reply(vec![bash("call_0", "ls")]);
    host.result(1, json!("a.txt"));
    host.reply(vec![bash("call_0", "ls -a")]);
    assert_eq!(host.sent().last(), Some(&call(2, "ls -a")));
    host.result(2, json!(". .. a.txt"));

    let messages = &host.asked.last().unwrap().messages;
    assert_eq!(messages[2].content, vec![told("call_0", "a.txt")]);
    assert_eq!(messages[4].content, vec![told("call_0", ". .. a.txt")]);
}

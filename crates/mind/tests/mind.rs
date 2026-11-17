//! What a mind does, driven the way a host drives it.

use mensor_mind::{
    Block, CLONE, Conflict, Content, Fork, HandRecord, Item, MemoryStore, Message, Mind,
    MindRecord, ModelReply, ModelRequest, Origin, Outcome, Output, Report, Role, Settings, State,
    Status, Stop, Store, Usage, tail_from,
};
use mensor_protocol::{
    Call, CallId, CallResult, CapabilityDecl, FromMind, Hello, Name, OperationDecl, Refusal, Reply,
};
use serde_json::{Value, json};

/// A host that performs outputs by writing them down.
struct Host {
    mind: Mind,
    stored: Vec<Item>,
    sent: Vec<(Name, FromMind)>,
    closed: Vec<(Name, Refusal)>,
    forks: Vec<Fork>,
    asked: Vec<ModelRequest>,
    /// Each message the mind said to another, as (to, key, text).
    told: Vec<(Name, String, String)>,
    /// Each list of minds the mind watched.
    watched: Vec<Vec<Name>>,
}

impl Host {
    fn new() -> Self {
        Self::with(Settings::default())
    }

    fn with(settings: Settings) -> Self {
        Self::around(Mind::new(name("m"), settings), Vec::new())
    }

    fn around(mind: Mind, stored: Vec<Item>) -> Self {
        Self {
            mind,
            stored,
            sent: Vec::new(),
            closed: Vec::new(),
            forks: Vec::new(),
            asked: Vec::new(),
            told: Vec::new(),
            watched: Vec::new(),
        }
    }

    /// Performs outputs, checking that a call is stored before it is sent,
    /// and a fork made before anything after it is stored.
    fn perform(&mut self, outputs: Vec<Output>) {
        for output in outputs {
            match output {
                Output::Append(item) => self.stored.push(item),
                Output::Send { hand, message } => {
                    if let FromMind::Call(call) = &message {
                        let stored = self.stored.iter().any(|item| {
                            matches!(item, Item::Call { id, hand: to, .. } if *id == call.id && *to == hand)
                        });
                        assert!(stored, "call {} was sent before it was stored", call.id);
                    }
                    self.sent.push((hand, message));
                }
                Output::Close { hand, refusal } => self.closed.push((hand, refusal)),
                Output::Fork(fork) => self.forks.push(fork),
                Output::Ask(request) => self.asked.push(request),
                Output::Tell { to, key, text } => self.told.push((to, key, text)),
                Output::Watch(minds) => self.watched.push(minds),
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
        let outputs = self.mind.attach(hello).expect("the hand is refused");
        self.perform(outputs);
    }

    fn prompt(&mut self, text: &str) {
        let prompted = self.mind.prompt(None, text.into(), None, None).unwrap();
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
        self.result_from("a1", id, Reply::Output(output));
    }

    fn result_from(&mut self, hand: &str, id: CallId, reply: Reply) {
        let outputs = self.mind.result(&name(hand), CallResult { id, reply });
        self.perform(outputs);
    }

    /// What was sent since last asked, to whichever hand.
    fn sent(&mut self) -> Vec<FromMind> {
        let sent = std::mem::take(&mut self.sent);
        sent.into_iter().map(|(_, message)| message).collect()
    }

    /// What was sent since last asked, with the hand each went to.
    fn sent_to(&mut self) -> Vec<(String, FromMind)> {
        let sent = std::mem::take(&mut self.sent);
        sent.into_iter()
            .map(|(hand, message)| (hand.to_string(), message))
            .collect()
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
    hand("a1", boot)
}

/// A hand of the mind `m` that runs commands.
fn hand(hand: &str, boot: &str) -> Hello {
    Hello {
        mind: name("m"),
        hand: name(hand),
        boot: boot.into(),
        last_id: 0,
        running: Vec::new(),
        held: Vec::new(),
        capabilities: vec![capability("commands", "bash")],
    }
}

/// A hand that runs commands and can clone itself.
fn cloning(hand: &str, boot: &str) -> Hello {
    let mut hello = self::hand(hand, boot);
    hello.capabilities.push(capability(CLONE, CLONE));
    hello
}

fn capability(name: &str, operation: &str) -> CapabilityDecl {
    CapabilityDecl {
        name: name.into(),
        operations: vec![OperationDecl {
            name: operation.into(),
            description: format!("{name} {operation}"),
            input: json!({"type": "object"}),
        }],
    }
}

/// What the model is told when a hand that runs commands joins.
fn joined(hand: &str) -> Content {
    Content::Text(format!("(Hand {hand} joined. It can: bash.)"))
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
            hand: name("a1"),
            boot: "boot-1".into(),
            capabilities: hello("boot-1").capabilities,
        }]
    );
    assert_eq!(host.mind.state(), State::Idle);
    assert_eq!(host.mind.connected(), [name("a1")]);

    host.prompt("list the files");
    let request = host.asked.last().unwrap();
    assert_eq!(
        request.messages,
        vec![Message {
            role: Role::User,
            content: vec![joined("a1"), Content::Text("list the files".into())]
        }]
    );
    assert_eq!(request.tools.len(), 1);
    assert_eq!(request.tools[0].name, "bash");
    assert_eq!(
        request.tools[0].input["properties"]["hand"]["type"],
        "string"
    );
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
    let keyed =
        |mind: &mut Mind, text: &str| mind.prompt(Some("k1".into()), text.into(), None, None);
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
    host.mind.detach(&name("a1"));
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
    host.mind.detach(&name("a1"));
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
    host.mind.detach(&name("a1"));
    host.attach(&Hello {
        last_id: 1,
        held: vec![held(1, json!("held"))],
        ..hello("boot-1")
    });
    assert_eq!(host.sent(), vec![ack(&[1])]);
    assert_eq!(host.last_message().content, vec![told("tu_1", "held")]);

    // The hand never read it: send it again under the same id.
    let mut host = waiting_on_a_call();
    host.mind.detach(&name("a1"));
    host.attach(&hello("boot-1"));
    assert_eq!(host.sent(), vec![call(1, "ls")]);
    assert_eq!(host.asked(), 1);

    // The hand read it and holds nothing: unknown, and never sent again.
    let mut host = waiting_on_a_call();
    host.mind.detach(&name("a1"));
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
    host.mind.detach(&name("a1"));
    host.attach(&hello("boot-2"));
    assert!(host.sent().is_empty());
    assert!(
        matches!(outcome(&host.stored, "tu_1"), Outcome::Unknown(why) if why.contains("restarted"))
    );
    assert!(matches!(
        host.stored.last(),
        Some(Item::Attached { hand, boot, .. }) if *hand == name("a1") && boot == "boot-2"
    ));
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
        Mind::load(name("m"), 0, before.stored.clone(), Settings::default()),
        before.stored,
    );
    assert_eq!(host.mind.state(), State::Working);
    assert!(host.mind.connected().is_empty());

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
        Mind::load(name("m"), 0, before.stored.clone(), Settings::default()),
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
            joined("a1"),
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
            joined("a1"),
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

    host.result_from("a1", 1, Reply::Error("cancelled".into()));
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
    host.mind.detach(&name("a1"));
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
    host.mind.detach(&name("a1"));
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

    host.result_from("a1", 1, Reply::Error("cancelled".into()));
    assert_eq!(host.mind.state(), State::Idle);
    assert_eq!(host.asked(), 1);
}

#[test]
fn a_call_the_hand_never_read_does_not_run_once_its_turn_is_interrupted() {
    let mut host = waiting_on_a_call();
    host.mind.detach(&name("a1"));
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
    let mut host = Host::with(compact_at_100());
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

/// What the model is told of its hands after a summary.
fn hands_line() -> Content {
    Content::Text("(Your hands: a1 (bash).)".into())
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
            hands: [(
                name("a1"),
                HandRecord {
                    boot: "boot-1".into(),
                    capabilities: hello("boot-1").capabilities,
                    released: false,
                    cloned: false,
                }
            )]
            .into(),
            last_call: 1,
            forks_left: 64,
            parent: None,
            children: Vec::new(),
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
            content: vec![summary_of("Listed the files: a.txt."), hands_line()]
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
    let mut host = Host::with(settings);
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
            hands_line(),
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

    let mind = Mind::load(name("m"), 0, host.stored.clone(), compact_at_100());
    let mut host = Host::around(mind, host.stored);
    host.attach(&hello("boot-1"));
    assert_eq!(
        host.last_message().content,
        vec![
            summary_of("We greeted each other."),
            hands_line(),
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
    let mind = Mind::load(name("m"), from, tail, compact_at_100());
    let mut host = Host::around(mind, before.stored);
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
        vec![summary_of("Listed the files: a.txt."), hands_line()]
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
        // Told from a store, no hand is connected: a call still out waits
        // for its hand.
        let told = told_from_store(&host.stored);
        assert_eq!(
            (told.state, told.length),
            (host.mind.state(), host.mind.length())
        );
        if told.state == State::Working {
            assert_eq!(told.waiting, [name("a1")]);
        }
    }
}

#[test]
fn a_key_from_before_the_items_a_mind_holds_is_one_its_host_looks_up() {
    let mut host = summed_up();
    let found = Some("hello");
    let again = host
        .mind
        .prompt(Some("k1".into()), "hello".into(), None, found)
        .unwrap();
    assert!(again.existing && again.outputs.is_empty());
    assert_eq!(
        host.mind
            .prompt(Some("k1".into()), "other".into(), None, found),
        Err(Conflict { key: "k1".into() })
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

/// A host whose mind has hands `a1` and `b2`, both connected.
fn two_hands() -> Host {
    let mut host = Host::new();
    host.attach(&hand("a1", "boot-a"));
    host.attach(&hand("b2", "boot-b"));
    host
}

#[test]
fn calls_to_different_hands_run_at_once_and_one_hands_keep_their_order() {
    let mut host = two_hands();
    host.prompt("build both");
    host.reply(vec![
        on("tu_1", "a1", "cargo build"),
        on("tu_2", "a1", "cargo test"),
        on("tu_3", "b2", "make"),
    ]);
    assert_eq!(
        host.sent_to(),
        vec![
            ("a1".into(), call(1, "cargo build")),
            ("b2".into(), call(2, "make")),
        ]
    );
    host.result_from("b2", 2, Reply::Output(json!("made")));
    assert_eq!(host.sent_to(), vec![("b2".into(), ack(&[2]))]);
    host.result(1, json!("built"));
    assert_eq!(
        host.sent_to(),
        vec![
            ("a1".into(), ack(&[1])),
            ("a1".into(), call(3, "cargo test")),
        ]
    );
    assert_eq!(host.asked(), 1);
    host.result(3, json!("passed"));
    assert_eq!(
        host.last_message().content,
        vec![
            told("tu_1", "built"),
            told("tu_2", "passed"),
            told("tu_3", "made")
        ]
    );
}

#[test]
fn a_result_is_taken_only_from_the_hand_the_call_went_to() {
    let mut host = two_hands();
    host.prompt("build");
    host.reply(vec![on("tu_1", "a1", "cargo build")]);
    host.sent();
    host.result_from("b2", 1, Reply::Output(json!("not mine")));
    assert_eq!(host.sent_to(), vec![("b2".into(), ack(&[1]))]);
    assert_eq!(host.mind.state(), State::Working);
}

#[test]
fn a_tool_use_says_which_hand_when_there_are_several() {
    let mut host = two_hands();
    host.prompt("go");
    host.reply(vec![
        bash("tu_1", "ls"),
        on("tu_2", "c3", "ls"),
        Block::ToolUse {
            id: "tu_3".into(),
            name: "bash".into(),
            input: json!({"command": "ls", "hand": 7}),
        },
    ]);
    assert!(host.sent().is_empty());
    assert!(
        matches!(outcome(&host.stored, "tu_1"), Outcome::NotRun(why) if why.contains("say which hand") && why.contains("a1, b2"))
    );
    assert!(
        matches!(outcome(&host.stored, "tu_2"), Outcome::NotRun(why) if why.contains("no hand named c3"))
    );
    assert!(matches!(outcome(&host.stored, "tu_3"), Outcome::NotRun(_)));
    assert_eq!(host.asked(), 2);
}

#[test]
fn a_tool_a_hand_lacks_is_not_run_there() {
    let mut host = Host::new();
    host.attach(&hand("a1", "boot-a"));
    let mut fetching = hand("w1", "boot-w");
    fetching.capabilities = vec![capability("fetch", "fetch")];
    host.attach(&fetching);
    host.prompt("go");
    let tools: Vec<String> = host.asked[0]
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect();
    assert_eq!(tools, ["bash", "fetch"]);
    host.reply(vec![on("tu_1", "w1", "ls")]);
    assert!(
        matches!(outcome(&host.stored, "tu_1"), Outcome::NotRun(why) if why.contains("w1 cannot do bash") && why.contains("a1"))
    );
}

#[test]
fn a_tool_use_that_names_no_hand_means_the_one_the_mind_had_when_it_was_asked() {
    let mut host = Host::new();
    host.attach(&hello("boot-1"));
    host.prompt("list");
    host.mind.detach(&name("a1"));
    host.reply(vec![bash("tu_1", "ls")]);
    // A second hand joins before the first is back: the step still meant a1.
    host.attach(&hand("b2", "boot-b"));
    assert!(host.sent().is_empty());
    host.attach(&hello("boot-1"));
    assert_eq!(host.sent_to(), vec![("a1".into(), call(1, "ls"))]);
}

#[test]
fn a_hand_that_is_away_holds_up_only_its_own_tool_uses() {
    let mut host = two_hands();
    host.mind.detach(&name("b2"));
    host.prompt("both");
    host.reply(vec![on("tu_1", "a1", "ls"), on("tu_2", "b2", "ls")]);
    assert_eq!(host.sent_to(), vec![("a1".into(), call(1, "ls"))]);
    // Nothing is stored for b2 until it is back.
    let calls = host
        .stored
        .iter()
        .filter(|item| matches!(item, Item::Call { .. }))
        .count();
    assert_eq!(calls, 1);
    assert_eq!(host.mind.waiting(), [name("b2")]);

    host.result(1, json!("a"));
    assert_eq!(host.asked(), 1);
    host.sent();
    host.attach(&hand("b2", "boot-b"));
    assert_eq!(host.sent_to(), vec![("b2".into(), call(2, "ls"))]);
    host.result_from("b2", 2, Reply::Output(json!("b")));
    assert_eq!(host.asked(), 2);
}

#[test]
fn an_interrupt_ends_what_waits_for_an_absent_hand() {
    let mut host = two_hands();
    host.mind.detach(&name("b2"));
    host.prompt("both");
    host.reply(vec![on("tu_1", "a1", "sleep 9"), on("tu_2", "b2", "ls")]);
    host.sent();
    host.interrupt();
    assert_eq!(
        host.sent_to(),
        vec![("a1".into(), FromMind::Cancel { id: 1 })]
    );
    assert!(
        matches!(outcome(&host.stored, "tu_2"), Outcome::NotRun(why) if why.contains("interrupted"))
    );
    host.result(1, json!("stopped"));
    assert_eq!(host.mind.state(), State::Idle);
    assert!(host.mind.waiting().is_empty());
}

#[test]
fn a_hand_that_restarts_makes_only_its_own_calls_unknown() {
    let mut host = two_hands();
    host.prompt("both");
    host.reply(vec![on("tu_1", "a1", "ls"), on("tu_2", "b2", "ls")]);
    host.mind.detach(&name("a1"));
    host.attach(&hand("a1", "boot-a2"));
    assert!(
        matches!(outcome(&host.stored, "tu_1"), Outcome::Unknown(why) if why.contains("restarted"))
    );
    assert_eq!(host.mind.state(), State::Working, "b2's call is still out");
    host.result_from("b2", 2, Reply::Output(json!("b")));
    assert_eq!(host.asked(), 2);
}

#[test]
fn releasing_a_dead_hand_settles_its_calls_and_frees_the_turn() {
    let mut host = two_hands();
    host.prompt("both");
    host.reply(vec![on("tu_1", "a1", "ls"), on("tu_2", "b2", "ls")]);
    host.result_from("b2", 2, Reply::Output(json!("b")));
    host.mind.detach(&name("a1"));
    assert_eq!(host.mind.waiting(), [name("a1")]);

    let outputs = host.mind.release(&name("a1")).unwrap();
    host.perform(outputs);
    assert!(
        matches!(outcome(&host.stored, "tu_1"), Outcome::Unknown(why) if why.contains("released"))
    );
    assert!(
        host.stored
            .iter()
            .any(|item| matches!(item, Item::Released { hand, .. } if *hand == name("a1")))
    );
    assert_eq!(host.closed, [(name("a1"), Refusal::Released)]);
    assert_eq!(host.asked(), 2, "the turn goes on with b2");
    assert!(
        format!("{:?}", host.last_message())
            .contains("(Hand a1 was let go: it was released from outside the conversation.)")
    );

    // The hand is refused from now on, and is no longer offered.
    assert_eq!(
        host.mind.attach(&hand("a1", "boot-a")).unwrap_err(),
        Refusal::Released
    );
    assert!(host.mind.release(&name("a1")).is_err());
}

#[test]
fn a_mind_takes_every_hand_that_joins_by_itself() {
    let mut host = Host::with(Settings {
        max_clones: 0,
        ..Settings::default()
    });
    for (hand_name, boot) in [
        ("a1", "a"),
        ("b2", "b"),
        ("c3", "c"),
        ("d4", "d"),
        ("e5", "e"),
    ] {
        host.attach(&hand(hand_name, boot));
    }
    // One it has may come back.
    host.mind.detach(&name("a1"));
    host.attach(&hand("a1", "a"));
}

#[test]
fn the_tools_stay_the_same_as_hands_come_and_go() {
    let mut host = Host::new();
    host.attach(&hand("a1", "boot-a"));
    host.prompt("one");
    host.reply(vec![text("ok")]);
    host.attach(&hand("b2", "boot-b"));
    host.prompt("two");
    assert_eq!(host.asked[0].tools, host.asked[1].tools);
    assert_eq!(
        host.last_message().content,
        vec![joined("b2"), Content::Text("two".into())]
    );
}

#[test]
fn a_summary_carries_the_hands_to_a_mind_given_only_its_end() {
    let mut host = Host::with(compact_at_100());
    host.attach(&hand("a1", "boot-a"));
    host.attach(&hand("b2", "boot-b"));
    let outputs = host.mind.release(&name("b2")).unwrap();
    host.perform(outputs);
    host.prompt("hello");
    host.reply_costing(vec![text("hi")], costing(95, 5));
    host.prompt("again");
    host.reply(vec![text("We greeted each other.")]);

    let from = tail_from(&host.stored);
    let tail = host.stored[usize::try_from(from).unwrap()..].to_vec();
    let mut later = Host::around(
        Mind::load(name("m"), from, tail, compact_at_100()),
        host.stored,
    );
    // b2 was released before the summary: it is still refused.
    assert_eq!(
        later.mind.attach(&hand("b2", "boot-b")).unwrap_err(),
        Refusal::Released
    );
    later.attach(&hand("a1", "boot-a"));
    let asked = later.asked.last().unwrap();
    assert_eq!(asked.tools.len(), 1);
    assert_eq!(
        later.last_message().content,
        vec![
            summary_of("We greeted each other."),
            hands_line(),
            Content::Text("again".into())
        ]
    );
}

#[test]
fn the_minds_own_tools_are_offered_with_a_hand_that_can_clone() {
    let mut host = Host::new();
    host.attach(&hello("boot-1"));
    host.prompt("hi");
    assert_eq!(host.asked[0].tools.len(), 1);

    let mut host = Host::new();
    host.attach(&cloning("a1", "boot-1"));
    host.prompt("hi");
    let tools: Vec<String> = host.asked[0]
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect();
    assert_eq!(tools, ["bash", "fork", "clone_hand", "release_hand"]);
    assert_eq!(
        host.last_message().content[0],
        Content::Text("(Hand a1 joined. It can: bash, and be cloned.)".into())
    );
}

fn own(id: &str, tool: &str, input: Value) -> Block {
    Block::ToolUse {
        id: id.into(),
        name: tool.into(),
        input,
    }
}

/// The clone call the mind sent `hand`, by id and input.
fn clone_call(host: &mut Host, hand: &str) -> (CallId, Value) {
    let sent = host.sent_to();
    sent.into_iter()
        .find_map(|(to, message)| match message {
            FromMind::Call(Call {
                id,
                capability,
                input,
                ..
            }) if to == hand && capability == CLONE => Some((id, input)),
            _ => None,
        })
        .expect("no clone call was sent")
}

#[test]
fn a_hand_is_cloned_into_the_mind_once_its_step_has_run() {
    let mut host = Host::new();
    host.attach(&cloning("a1", "boot-a"));
    host.attach(&hand("b2", "boot-b"));
    host.prompt("work in two places");
    host.reply(vec![
        own("tu_1", "clone_hand", json!({"hand": "a1"})),
        on("tu_2", "b2", "ls"),
    ]);
    // The mind's own tool waits for the rest of its step.
    assert_eq!(host.sent_to(), vec![("b2".into(), call(1, "ls"))]);
    host.result_from("b2", 1, Reply::Output(json!("b")));
    let (id, input) = clone_call(&mut host, "a1");
    assert_eq!(
        (id, input),
        (2, json!({"into": [{"mind": "m", "hand": "a1-2"}]}))
    );

    host.result_from("a1", 2, Reply::Output(json!({"clones": 1})));
    assert_eq!(
        outcome(&host.stored, "tu_1"),
        &Outcome::Output(json!({"hand": "a1-2"}))
    );
    assert!(host.forks.is_empty());
    host.sent();

    // The clone joins, and works beside its original.
    host.attach(&cloning("a1-2", "boot-c"));
    host.reply(vec![on("tu_3", "a1", "ls"), on("tu_4", "a1-2", "ls")]);
    assert_eq!(
        host.sent_to(),
        vec![("a1".into(), call(3, "ls")), ("a1-2".into(), call(4, "ls"))]
    );
}

#[test]
fn a_clone_is_not_made_for_a_mind_with_all_its_clones() {
    let mut host = Host::with(Settings {
        max_clones: 1,
        ..Settings::default()
    });
    host.attach(&cloning("a1", "boot-a"));
    host.attach(&hand("b2", "boot-b"));
    host.prompt("clone twice");
    host.reply(vec![own("tu_1", "clone_hand", json!({}))]);
    let (id, _) = clone_call(&mut host, "a1");
    host.result_from("a1", id, Reply::Output(json!({"clones": 1})));
    assert_eq!(
        outcome(&host.stored, "tu_1"),
        &Outcome::Output(json!({"hand": "a1-2"}))
    );
    host.sent();

    // b2 joined by itself and is not counted. The clone is.
    host.reply(vec![own("tu_2", "clone_hand", json!({"hand": "a1"}))]);
    assert!(host.sent().is_empty());
    assert!(
        matches!(outcome(&host.stored, "tu_2"), Outcome::NotRun(why) if why.contains("all the clones it may: 1"))
    );

    // Released, it no longer counts.
    host.reply(vec![own("tu_3", "release_hand", json!({"hand": "a1-2"}))]);
    host.reply(vec![own("tu_4", "clone_hand", json!({"hand": "a1"}))]);
    clone_call(&mut host, "a1");
}

#[test]
fn a_clone_that_says_hello_before_its_result_still_counts() {
    let mut host = Host::with(Settings {
        max_clones: 1,
        ..Settings::default()
    });
    host.attach(&cloning("a1", "boot-a"));
    host.prompt("clone twice");
    host.reply(vec![own("tu_1", "clone_hand", json!({}))]);
    let (id, _) = clone_call(&mut host, "a1");
    host.attach(&cloning("a1-2", "boot-c"));
    host.result_from("a1", id, Reply::Output(json!({"clones": 1})));
    host.sent();

    host.reply(vec![own("tu_2", "clone_hand", json!({"hand": "a1"}))]);
    assert!(host.sent().is_empty());
    assert!(
        matches!(outcome(&host.stored, "tu_2"), Outcome::NotRun(why) if why.contains("all the clones it may: 1"))
    );
}

#[test]
fn a_child_does_not_count_the_clone_it_was_forked_onto() {
    let settings = || Settings {
        max_clones: 1,
        ..Settings::default()
    };
    let mut host = Host::with(settings());
    host.attach(&cloning("a1", "boot-a"));
    host.prompt("clone, then fork the clone");
    host.reply(vec![own("tu_1", "clone_hand", json!({}))]);
    let (id, _) = clone_call(&mut host, "a1");
    host.result_from("a1", id, Reply::Output(json!({"clones": 1})));
    host.attach(&cloning("a1-2", "boot-c"));
    host.sent();
    host.reply(vec![own(
        "tu_2",
        "fork",
        json!({"hand": "a1-2", "prompts": ["go"]}),
    )]);
    let (id, _) = clone_call(&mut host, "a1-2");
    host.result_from("a1-2", id, Reply::Output(json!({})));

    let fork = host.forks[0].clone();
    let mut stored = host.stored[..usize::try_from(fork.inherited).unwrap()].to_vec();
    stored.extend(fork.items.clone());
    let mut child = Host::around(
        Mind::load(fork.name.clone(), 0, stored.clone(), settings()),
        stored,
    );
    child.attach(&Hello {
        mind: fork.name.clone(),
        ..cloning("a1-2", "boot-d")
    });
    child.reply(vec![own("tu_3", "clone_hand", json!({}))]);
    clone_call(&mut child, "a1-2");
}

#[test]
fn a_hand_is_let_go_by_the_model() {
    let mut host = Host::new();
    host.attach(&cloning("a1", "boot-a"));
    host.attach(&hand("b2", "boot-b"));
    host.prompt("let b2 go");
    host.reply(vec![own("tu_1", "release_hand", json!({"hand": "b2"}))]);
    assert_eq!(
        outcome(&host.stored, "tu_1"),
        &Outcome::Output(json!({"released": "b2"}))
    );
    assert_eq!(host.closed, [(name("b2"), Refusal::Released)]);
    assert_eq!(host.asked(), 2);
}

#[test]
fn the_minds_own_tools_run_one_at_a_time_and_a_fork_runs_alone() {
    let mut host = Host::new();
    host.attach(&cloning("a1", "boot-a"));
    host.attach(&hand("b2", "boot-b"));
    host.prompt("clone, then let go");
    host.reply(vec![
        own("tu_1", "clone_hand", json!({})),
        own("tu_2", "release_hand", json!({"hand": "b2"})),
    ]);
    // The second waits for the first.
    let (id, _) = clone_call(&mut host, "a1");
    assert!(host.closed.is_empty());
    host.result_from("a1", id, Reply::Output(json!({})));
    assert!(matches!(outcome(&host.stored, "tu_1"), Outcome::Output(_)));
    assert!(matches!(outcome(&host.stored, "tu_2"), Outcome::Output(_)));
    assert_eq!(host.closed, [(name("b2"), Refusal::Released)]);

    host.reply(vec![
        own("tu_3", "release_hand", json!({"hand": "a1-2"})),
        own("tu_4", "fork", json!({"hand": "a1", "prompts": ["x"]})),
    ]);
    assert!(
        matches!(outcome(&host.stored, "tu_3"), Outcome::NotRun(why) if why.contains("forks runs none"))
    );
    clone_call(&mut host, "a1");
}

/// A host whose mind has forked from `a1` into two children, and holds
/// the clone call's result unacked.
fn forking() -> (Host, CallId) {
    let mut host = Host::new();
    host.attach(&cloning("a1", "boot-a"));
    host.attach(&hand("b2", "boot-b"));
    host.prompt("try two ways");
    host.reply(vec![own(
        "tu_1",
        "fork",
        json!({"hand": "a1", "prompts": ["the first way", "the second way"]}),
    )]);
    let (id, input) = clone_call(&mut host, "a1");
    let into = input["into"].as_array().unwrap();
    assert_eq!(into.len(), 2);
    for target in into {
        assert_eq!(target["hand"], "a1");
        assert!(target["mind"].as_str().unwrap().starts_with("m."));
    }
    (host, id)
}

#[test]
fn a_fork_makes_its_children_before_it_is_stored() {
    let (mut host, id) = forking();
    let calls = host.stored.len();
    let outputs = host.mind.result(
        &name("a1"),
        CallResult {
            id,
            reply: Reply::Output(json!({})),
        },
    );

    // The children first, then the fork, then the ack.
    assert!(matches!(&outputs[0], Output::Fork(_)));
    assert!(matches!(&outputs[1], Output::Fork(_)));
    assert!(matches!(&outputs[2], Output::Append(Item::Outcome { .. })));
    assert!(matches!(
        &outputs[3],
        Output::Append(Item::Forked { left: 22, .. })
    ));
    assert!(matches!(
        &outputs[4],
        Output::Send {
            message: FromMind::Ack { .. },
            ..
        }
    ));
    host.perform(outputs);

    let children: Vec<Name> = host.forks.iter().map(|fork| fork.name.clone()).collect();
    assert_eq!(
        outcome(&host.stored, "tu_1"),
        &Outcome::Output(json!({"children": children}))
    );
    let first = &host.forks[0];
    assert_eq!(first.parent, name("m"));
    assert_eq!(first.inherited, u64::try_from(calls).unwrap());
    assert_eq!(
        first.items,
        vec![
            Item::Outcome {
                tool_use: "tu_1".into(),
                call: Some(id),
                outcome: Outcome::Output(json!({"you": children[0], "parent": "m"})),
            },
            Item::Released {
                hand: name("b2"),
                reason: "it stayed with m when this mind was forked from it".into(),
            },
            Item::Parted {
                parent: name("m"),
                left: 20
            },
            Item::Prompt {
                key: None,
                text: "the first way".into(),
                from: Some(name("m")),
            },
        ]
    );
    assert_eq!(
        host.stored.last(),
        Some(&Item::Forked {
            children: children.clone(),
            left: 22
        })
    );
    assert!(
        matches!(&host.forks[1].items[3], Item::Prompt { text, .. } if text == "the second way")
    );
    assert_eq!(host.asked(), 2, "the parent carries on");
}

#[test]
fn a_child_works_once_its_clone_says_hello() {
    let (mut host, id) = forking();
    host.result_from("a1", id, Reply::Output(json!({})));
    let fork = host.forks[0].clone();
    let mut stored = host.stored[..usize::try_from(fork.inherited).unwrap()].to_vec();
    stored.extend(fork.items.clone());
    let mut child = Host::around(
        Mind::load(fork.name.clone(), 0, stored.clone(), Settings::default()),
        stored,
    );
    assert_eq!(child.mind.state(), State::Thinking);
    assert_eq!(child.asked(), 0);

    // The clone is a new process in the child: its boot is new.
    let clone = Hello {
        mind: fork.name.clone(),
        ..cloning("a1", "boot-clone")
    };
    child.attach(&clone);
    assert_eq!(child.asked(), 1);
    // b2 stayed with the parent, and is refused here.
    assert_eq!(
        child.mind.attach(&hand("b2", "boot-b")).unwrap_err(),
        Refusal::Released
    );
    let content = &child.last_message().content;
    assert!(
        matches!(&content[0], Content::ToolResult { tool_use, text, .. } if tool_use == "tu_1" && text.contains("you"))
    );
    assert_eq!(
        content[1..],
        [
            Content::Text(
                "(Hand b2 was let go: it stayed with m when this mind was forked from it.)".into()
            ),
            Content::Text("(From your parent m:)\n\nthe first way".into()),
        ]
    );
    child.reply(vec![bash("tu_2", "ls")]);
    assert_eq!(child.sent_to(), vec![("a1".into(), call(id + 1, "ls"))]);
}

#[test]
fn a_fork_whose_result_comes_again_after_a_stop_is_made_once() {
    let (host, id) = forking();
    // The host stopped after making the children, before storing the fork:
    // its mind is loaded again, and a1 says hello holding the result.
    let mut again = Host::around(
        Mind::load(name("m"), 0, host.stored.clone(), Settings::default()),
        host.stored.clone(),
    );
    again.attach(&hand("b2", "boot-b"));
    again.attach(&Hello {
        last_id: id,
        held: vec![held(id, json!({}))],
        ..cloning("a1", "boot-a")
    });
    assert_eq!(again.forks.len(), 2);
    assert!(
        matches!(outcome(&again.stored, "tu_1"), Outcome::Output(value) if value["children"].is_array())
    );
    assert_eq!(again.sent_to().last(), Some(&("a1".into(), ack(&[id]))));
}

#[test]
fn a_clone_that_fails_makes_no_child() {
    let (mut host, id) = forking();
    host.result_from("a1", id, Reply::Error("the disk is full".into()));
    assert!(host.forks.is_empty());
    assert_eq!(
        outcome(&host.stored, "tu_1"),
        &Outcome::Error("the disk is full".into())
    );
    assert!(
        !host
            .stored
            .iter()
            .any(|item| matches!(item, Item::Forked { .. }))
    );
}

#[test]
fn a_mind_forks_no_more_than_its_budget_allows() {
    let mut host = Host::with(Settings {
        max_forks: 1,
        ..Settings::default()
    });
    host.attach(&cloning("a1", "boot-a"));
    host.prompt("fork");
    host.reply(vec![own("tu_1", "fork", json!({"prompts": ["x", "y"]}))]);
    assert!(
        matches!(outcome(&host.stored, "tu_1"), Outcome::NotRun(why) if why.contains("may fork 1 more"))
    );
    host.reply(vec![own("tu_2", "fork", json!({"prompts": ["x"]}))]);
    let (id, _) = clone_call(&mut host, "a1");
    host.result_from("a1", id, Reply::Output(json!({})));
    assert_eq!(
        host.forks[0].items[1],
        Item::Parted {
            parent: name("m"),
            left: 0
        }
    );
    assert!(matches!(
        host.stored.last(),
        Some(Item::Forked { left: 0, .. })
    ));
}

fn reading(hand: &str, boot: &str, capability: &str) -> Hello {
    Hello {
        capabilities: vec![self::capability(capability, "read")],
        ..self::hand(hand, boot)
    }
}

#[test]
fn a_call_out_is_not_ended_when_hands_coming_rename_its_tool() {
    let mut host = Host::new();
    host.attach(&reading("a1", "boot-a", "files"));
    host.prompt("read it");
    host.reply(vec![
        own("tu_1", "read", json!({})),
        own("tu_2", "read", json!({})),
    ]);
    assert_eq!(host.sent().len(), 1);
    // b2's `notes.read` renames `read` to `files_read` while call 1 runs.
    host.attach(&reading("b2", "boot-b", "notes"));
    assert_eq!(host.mind.state(), State::Working);
    assert!(
        !host
            .stored
            .iter()
            .any(|item| matches!(item, Item::Outcome { tool_use, .. } if tool_use == "tu_1"))
    );
    host.result(1, json!("one"));
    assert_eq!(
        outcome(&host.stored, "tu_1"),
        &Outcome::Output(json!("one"))
    );
    // The use that had not become a call still runs, on a1, after it.
    assert_eq!(
        host.sent_to().last(),
        Some(&(
            "a1".into(),
            FromMind::Call(Call {
                id: 2,
                capability: "files".into(),
                operation: "read".into(),
                input: json!({}),
            })
        ))
    );
}

#[test]
fn a_child_is_let_go_of_the_hands_its_parent_had_at_the_fork() {
    let (mut host, id) = forking();
    // While the clone runs, b2 is let go and c3 joins.
    let outputs = host.mind.release(&name("b2")).unwrap();
    host.perform(outputs);
    host.attach(&hand("c3", "boot-c"));
    host.result_from("a1", id, Reply::Output(json!({})));
    let released: Vec<&Name> = host.forks[0]
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Released { hand, .. } => Some(hand),
            _ => None,
        })
        .collect();
    assert_eq!(released, [&name("b2")]);
}

#[test]
fn a_resting_mind_says_which_hand_its_turn_waits_for() {
    let mut host = two_hands();
    host.prompt("both");
    host.mind.detach(&name("b2"));
    host.reply(vec![on("tu_1", "a1", "ls"), on("tu_2", "b2", "ls")]);
    host.result(1, json!("a"));
    // Told from its latest items, as a store gives them.
    let status = told_from_store(&host.stored);
    assert_eq!(
        (status.state, status.waiting),
        (State::Working, vec![name("b2")])
    );
}

#[test]
fn a_mind_whose_hands_are_all_let_go_still_talks() {
    let mut host = waiting_on_a_call();
    host.mind.detach(&name("a1"));
    let outputs = host.mind.release(&name("a1")).unwrap();
    host.perform(outputs);
    assert_eq!(host.asked(), 2, "the model hears how the call ended");
    host.reply(vec![text("My hand is gone.")]);
    host.prompt("still there?");
    assert_eq!(host.asked(), 3);
    assert!(host.asked.last().unwrap().tools.is_empty());
}

#[test]
fn a_mind_whose_hands_are_away_waits_for_any_of_them() {
    let mut host = two_hands();
    host.mind.detach(&name("a1"));
    host.mind.detach(&name("b2"));
    host.prompt("anyone?");
    assert_eq!(host.asked(), 0);
    assert_eq!(host.mind.state(), State::Thinking);
    assert_eq!(host.mind.waiting(), [name("a1"), name("b2")]);
}

#[test]
fn a_clone_made_into_the_mind_is_its_hand_before_it_says_hello() {
    let mut host = Host::new();
    host.attach(&cloning("a1", "boot-a"));
    host.prompt("clone, then use it");
    host.reply(vec![own("tu_1", "clone_hand", json!({}))]);
    let (id, _) = clone_call(&mut host, "a1");
    host.result_from("a1", id, Reply::Output(json!({})));
    assert!(host.stored.iter().any(
        |item| matches!(item, Item::Cloned { hand, from } if *hand == name("a1-2") && *from == name("a1"))
    ));
    // The model uses it at once: the call waits until it says hello.
    host.reply(vec![on("tu_2", "a1-2", "ls")]);
    host.sent();
    assert_eq!(host.mind.waiting(), [name("a1-2")]);
    // A second clone is not given its name.
    host.attach(&cloning("a1-2", "boot-c"));
    assert_eq!(host.sent_to(), vec![("a1-2".into(), call(id + 1, "ls"))]);
    assert_eq!(
        host.last_message().content.len(),
        1,
        "the result is still owed"
    );
    host.result_from("a1-2", id + 1, Reply::Output(json!("clone")));
    host.reply(vec![own("tu_3", "clone_hand", json!({"hand": "a1"}))]);
    let (_, input) = clone_call(&mut host, "a1");
    assert_eq!(input["into"][0]["hand"], "a1-3");
    assert!(format!("{:?}", host.asked[2].messages).contains("(Hand a1-2 joined."));
}

#[test]
fn a_hand_whose_operation_takes_a_hand_of_its_own_is_refused() {
    let mut host = Host::new();
    let mut odd = hand("a1", "boot-a");
    odd.capabilities[0].operations[0].input =
        json!({"type": "object", "properties": {"hand": {"type": "string"}}});
    assert_eq!(host.mind.attach(&odd).unwrap_err(), Refusal::Protocol);
}

#[test]
fn a_fork_whose_clone_may_have_run_names_its_children_and_spends_their_share() {
    let (host, id) = forking();
    // The host stopped, and a1 came back a new process that remembers
    // nothing: the children may or may not have been made.
    let mut again = Host::around(
        Mind::load(name("m"), 0, host.stored.clone(), Settings::default()),
        host.stored.clone(),
    );
    again.attach(&Hello {
        last_id: id,
        ..cloning("a1", "boot-a2")
    });
    let Outcome::Unknown(why) = outcome(&again.stored, "tu_1") else {
        panic!("not unknown");
    };
    assert!(why.contains("child minds m."), "{why}");
    assert!(
        again
            .stored
            .iter()
            .any(|item| matches!(item, Item::Forked { children, left: 22 } if children.len() == 2))
    );
}

#[test]
fn a_mind_loaded_after_a_fork_knows_its_budget() {
    let settings = Settings {
        max_forks: 3,
        ..Settings::default()
    };
    let mut host = Host::with(settings.clone());
    host.attach(&cloning("a1", "boot-a"));
    host.prompt("fork twice");
    host.reply(vec![own("tu_1", "fork", json!({"prompts": ["x", "y"]}))]);
    let (id, _) = clone_call(&mut host, "a1");
    host.result_from("a1", id, Reply::Output(json!({})));
    host.reply(vec![text("Forked.")]);

    // 1 remains of 3, and does not divide: the mind keeps it.
    let loaded = Mind::load(name("m"), 0, host.stored.clone(), settings);
    let mut later = Host::around(loaded, host.stored.clone());
    later.attach(&cloning("a1", "boot-a"));
    later.prompt("fork twice more");
    later.reply(vec![own("tu_2", "fork", json!({"prompts": ["x", "y"]}))]);
    assert!(
        matches!(outcome(&later.stored, "tu_2"), Outcome::NotRun(why) if why.contains("may fork 1 more"))
    );
}

/// A host whose mind has forked from `a1` into two children, and the
/// children's names.
fn forked() -> (Host, Vec<Name>) {
    let (mut host, id) = forking();
    host.result_from("a1", id, Reply::Output(json!({})));
    let children = host.forks.iter().map(|fork| fork.name.clone()).collect();
    (host, children)
}

/// The tools the model was last shown, by name.
fn shown(host: &Host) -> Vec<String> {
    let asked = host.asked.last().unwrap();
    asked.tools.iter().map(|tool| tool.name.clone()).collect()
}

/// The child the first of a fork's forks made, holding its history as a
/// store would, with its clone said hello.
fn first_child(host: &Host) -> Host {
    let fork = host.forks[0].clone();
    let mut stored = host.stored[..usize::try_from(fork.inherited).unwrap()].to_vec();
    stored.extend(fork.items.clone());
    let mut child = Host::around(
        Mind::load(fork.name.clone(), 0, stored.clone(), Settings::default()),
        stored,
    );
    child.attach(&Hello {
        mind: fork.name,
        ..cloning("a1", "boot-clone")
    });
    child
}

#[test]
fn a_parent_tells_a_child_and_waits_for_what_its_children_say() {
    let (mut host, children) = forked();
    assert_eq!(
        shown(&host),
        ["bash", "fork", "clone_hand", "release_hand", "tell", "wait"]
    );
    host.reply(vec![
        own(
            "tu_2",
            "tell",
            json!({"mind": children[0], "text": "and the tests?"}),
        ),
        own("tu_3", "wait", json!({})),
    ]);
    // The wait waits its turn behind the message.
    let (to, key, text) = host.told.pop().unwrap();
    assert_eq!((&to, text.as_str()), (&children[0], "and the tests?"));
    assert!(host.watched.is_empty());
    assert_eq!(host.mind.state(), State::Working);

    let outputs = host.mind.told(&key, Ok(()));
    host.perform(outputs);
    assert_eq!(
        outcome(&host.stored, "tu_2"),
        &Outcome::Output(json!({"told": children[0]}))
    );
    assert_eq!(host.watched, std::slice::from_ref(&children));
    assert_eq!(host.mind.watching(), children);

    let outputs = host
        .mind
        .rested(&children[1], Report::Failed("no model".into()));
    host.perform(outputs);
    assert_eq!(host.mind.watching(), [children[0].clone()]);
    let outputs = host
        .mind
        .rested(&children[0], Report::Said("They pass.".into()));
    host.perform(outputs);
    assert_eq!(
        outcome(&host.stored, "tu_3"),
        &Outcome::Output(json!({
            children[0].as_str(): {"said": "They pass."},
            children[1].as_str(): {"failed": "no model"},
        }))
    );
    assert_eq!(host.asked(), 3, "the parent hears what they said");
}

#[test]
fn a_message_from_kin_says_whose_it_is() {
    let (mut host, children) = forked();
    let mut child = first_child(&host);
    assert_eq!(
        shown(&child),
        ["bash", "fork", "clone_hand", "release_hand", "tell"]
    );
    child.reply(vec![own(
        "tu_9",
        "tell",
        json!({"mind": "m", "text": "Which way first?"}),
    )]);
    let (to, key, text) = child.told.pop().unwrap();
    assert_eq!(to, name("m"));

    // The parent's host gives the parent the child's message.
    host.reply(vec![text_block("Waiting.")]);
    let prompted = host
        .mind
        .prompt(Some(key), text, Some(children[0].clone()), None)
        .unwrap();
    host.perform(prompted.outputs);
    assert_eq!(
        host.last_message().content.last(),
        Some(&Content::Text(format!(
            "(From your child {}:)\n\nWhich way first?",
            children[0]
        )))
    );
}

fn text_block(said: &str) -> Block {
    text(said)
}

#[test]
fn a_mind_tells_only_its_parent_and_children_and_waits_only_for_children() {
    let (host, _) = forked();
    let mut child = first_child(&host);
    child.reply(vec![own(
        "tu_9",
        "tell",
        json!({"mind": "m.zzzzzzzz", "text": "hi"}),
    )]);
    assert!(
        matches!(outcome(&child.stored, "tu_9"), Outcome::NotRun(why) if why.contains("Your parent is m."))
    );
    child.reply(vec![own("tu_10", "wait", json!({}))]);
    assert!(
        matches!(outcome(&child.stored, "tu_10"), Outcome::NotRun(why) if why.contains("no tool named wait"))
    );
    assert!(child.told.is_empty() && child.watched.is_empty());
}

#[test]
fn a_mind_loaded_mid_wait_watches_again_and_says_a_message_again_as_the_same_prompt() {
    let (mut host, children) = forked();
    host.reply(vec![own(
        "tu_2",
        "tell",
        json!({"mind": children[1], "text": "go"}),
    )]);
    let (_, key, _) = host.told.pop().unwrap();

    // The host stopped before it heard the message arrive.
    let mut again = Host::around(
        Mind::load(name("m"), 0, host.stored.clone(), Settings::default()),
        host.stored.clone(),
    );
    let outputs = again.mind.start();
    again.perform(outputs);
    assert_eq!(
        again.told.pop().map(|(_, again, _)| again),
        Some(key.clone())
    );
    let outputs = again.mind.told(&key, Ok(()));
    again.perform(outputs);

    again.attach(&cloning("a1", "boot-a"));
    again.reply(vec![own("tu_3", "wait", json!({"minds": [children[1]]}))]);
    assert_eq!(again.watched, [vec![children[1].clone()]]);
    let mut later = Host::around(
        Mind::load(name("m"), 0, again.stored.clone(), Settings::default()),
        again.stored.clone(),
    );
    let status = told_from_store(&again.stored);
    assert_eq!(
        (status.state, status.watching, status.waiting),
        (State::Working, vec![children[1].clone()], Vec::new())
    );
    let outputs = later.mind.start();
    later.perform(outputs);
    assert_eq!(later.watched, [vec![children[1].clone()]]);
}

#[test]
fn an_interrupt_ends_a_wait_and_lets_a_message_on_its_way_arrive() {
    let (mut host, children) = forked();
    host.reply(vec![own("tu_2", "wait", json!({}))]);
    host.interrupt();
    assert!(matches!(outcome(&host.stored, "tu_2"), Outcome::NotRun(_)));
    let outputs = host.mind.rested(&children[0], Report::Said("late".into()));
    assert!(outputs.is_empty());

    host.prompt("tell the first");
    host.reply(vec![own(
        "tu_3",
        "tell",
        json!({"mind": children[0], "text": "stop"}),
    )]);
    let (_, key, _) = host.told.pop().unwrap();
    host.interrupt();
    assert!(
        !host
            .stored
            .iter()
            .any(|item| matches!(item, Item::Outcome { tool_use, .. } if tool_use == "tu_3"))
    );
    let outputs = host.mind.told(&key, Ok(()));
    host.perform(outputs);
    assert!(matches!(outcome(&host.stored, "tu_3"), Outcome::Output(_)));
    assert_eq!(host.mind.state(), State::Idle);
}

#[test]
fn a_mind_at_rest_reports_what_it_said_last() {
    let mut host = Host::new();
    host.attach(&hello("boot-1"));
    assert_eq!(host.mind.report(), Some(Report::Said(String::new())));
    host.prompt("hello");
    assert_eq!(host.mind.report(), None);
    host.reply(vec![text("Hi."), text("How can I help?")]);
    assert_eq!(
        told_from_store(&host.stored).report,
        Some(Report::Said("Hi.\n\nHow can I help?".into()))
    );
    host.prompt("list the files");
    host.reply(vec![bash("tu_1", "ls")]);
    host.interrupt();
    host.result(1, json!("cancelled"));
    assert_eq!(
        host.mind.report(),
        Some(Report::Paused("interrupted".into()))
    );
}

#[test]
fn a_summary_carries_a_minds_kin() {
    let (host, _) = forked();
    let mut child = first_child(&host);
    let settings = compact_at_100();
    child.reply_costing(vec![bash("tu_9", "ls")], costing(95, 10));
    let id = match child.sent_to().last() {
        Some((_, FromMind::Call(call))) => call.id,
        other => panic!("no call: {other:?}"),
    };
    child.mind = Mind::load(
        child.mind.name().clone(),
        0,
        child.stored.clone(),
        settings.clone(),
    );
    let outputs = child.mind.attach(&Hello {
        mind: child.mind.name().clone(),
        last_id: id,
        held: vec![held(id, json!("a.txt"))],
        ..cloning("a1", "boot-clone")
    });
    child.perform(outputs.unwrap());
    assert!(child.asked.last().unwrap().text_only);
    child.reply(vec![text("We looked.")]);
    let Some(Item::Summary {
        parent, children, ..
    }) = child.stored.last()
    else {
        panic!("no summary");
    };
    assert_eq!((parent, children), (&Some(name("m")), &Vec::new()));
    let told = &child.asked.last().unwrap().messages[0].content;
    assert!(told.contains(&Content::Text("(You were forked from m.)".into())));
    assert!(shown(&child).contains(&"tell".to_owned()));
}

#[test]
fn a_name_two_hands_now_know_by_other_tools_names_them() {
    let mut host = Host::new();
    host.attach(&reading("a1", "boot-a", "files"));
    host.attach(&reading("b2", "boot-b", "notes"));
    host.prompt("read it");
    assert_eq!(shown(&host), ["files_read", "notes_read"]);
    host.reply(vec![own("tu_1", "read", json!({}))]);
    assert!(
        matches!(outcome(&host.stored, "tu_1"), Outcome::NotRun(why) if why == "read now means more than one tool: files_read (a1), notes_read (b2). Ask for one of them by its name."),
        "{:?}",
        outcome(&host.stored, "tu_1")
    );
}

/// What a mind with the history `stored` is doing, told as its host tells
/// it of a mind that rests: from what a store gives as its recent items.
fn told_from_store(stored: &[Item]) -> Status {
    let store = MemoryStore::new();
    let record = MindRecord {
        name: name("m"),
        origin: Origin::Root,
    };
    store.create(&record, stored).unwrap();
    Mind::status(name("m"), store.recent(&name("m")).unwrap())
}

/// The child the fork `at` of a host's forks made, as its host would hold
/// it, with its clone said hello.
fn child_of(host: &Host, at: usize) -> Host {
    let fork = host.forks[at].clone();
    let mut stored = host.stored[..usize::try_from(fork.inherited).unwrap()].to_vec();
    stored.extend(fork.items.clone());
    let mut child = Host::around(
        Mind::load(fork.name.clone(), 0, stored.clone(), Settings::default()),
        stored,
    );
    child.attach(&Hello {
        mind: fork.name,
        ..cloning("a1", "boot-clone")
    });
    child
}

#[test]
fn a_child_of_a_later_fork_takes_no_sibling_for_its_child() {
    let (mut host, first) = forked();
    host.reply(vec![text("Forked.")]);
    host.prompt("once more");
    host.reply(vec![own(
        "tu_5",
        "fork",
        json!({"hand": "a1", "prompts": ["the third way"]}),
    )]);
    let (id, _) = clone_call(&mut host, "a1");
    host.result_from("a1", id, Reply::Output(json!({})));

    let mut third = child_of(&host, 2);
    assert_eq!(
        shown(&third),
        ["bash", "fork", "clone_hand", "release_hand", "tell"]
    );
    third.reply(vec![own(
        "tu_9",
        "tell",
        json!({"mind": first[0], "text": "hi"}),
    )]);
    assert!(
        matches!(outcome(&third.stored, "tu_9"), Outcome::NotRun(why) if why.contains("You have no children.")),
        "{:?}",
        outcome(&third.stored, "tu_9")
    );
    // The parent has all three.
    assert_eq!(host.mind.watching(), Vec::<Name>::new());
    host.reply(vec![own("tu_6", "wait", json!({}))]);
    assert_eq!(host.watched.last().unwrap().len(), 3);
}

#[test]
fn a_resting_parent_says_which_children_it_waits_for() {
    let (mut host, children) = forked();
    host.reply(vec![own("tu_2", "wait", json!({}))]);
    let status = told_from_store(&host.stored);
    assert_eq!(
        (status.state, status.watching, status.waiting),
        (State::Working, children, Vec::new())
    );
}

#[test]
fn a_fork_that_ended_does_not_end_the_minds_other_tools() {
    let (mut host, children) = forked();
    // a1, the hand that could clone, is let go: fork is no tool.
    let outputs = host.mind.release(&name("a1")).unwrap();
    host.perform(outputs);
    host.reply(vec![
        own("tu_2", "wait", json!({})),
        own("tu_3", "fork", json!({"hand": "b2", "prompts": ["x"]})),
    ]);
    assert!(matches!(outcome(&host.stored, "tu_3"), Outcome::NotRun(_)));
    assert_eq!(host.watched.len(), 1);
    // A hand that can clone joins: the fork that ended stays ended, and
    // the wait goes on.
    host.attach(&cloning("c3", "boot-c"));
    assert_eq!(host.mind.watching(), children);
    for child in &children {
        let outputs = host.mind.rested(child, Report::Said("done".into()));
        host.perform(outputs);
    }
    assert!(matches!(outcome(&host.stored, "tu_2"), Outcome::Output(_)));
}

#[test]
fn a_name_one_hand_now_knows_by_two_tools_names_them() {
    let mut host = Host::new();
    let mut both = reading("a1", "boot-a", "files");
    both.capabilities.push(capability("notes", "read"));
    host.attach(&both);
    host.prompt("read it");
    host.reply(vec![own("tu_1", "read", json!({"hand": "a1"}))]);
    assert_eq!(
        outcome(&host.stored, "tu_1"),
        &Outcome::NotRun(
            "read now means more than one tool: files_read (a1), notes_read (a1). Ask for one of \
             them by its name."
                .into()
        )
    );
}

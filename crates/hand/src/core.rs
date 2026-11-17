use std::collections::{BTreeSet, VecDeque};

use mensor_protocol::{
    Call, CallId, CallResult, CapabilityDecl, FromHand, FromMind, Hello, MAX_FRAME_BYTES, Name,
    Refused, Reply,
};
use serde::{Deserialize, Serialize};

/// The bounds a hand keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    /// The most bytes of results a hand holds, as they are written in
    /// messages. Held results travel in one `hello`, so this stays below
    /// [`MAX_FRAME_BYTES`].
    pub max_held_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_held_bytes: MAX_FRAME_BYTES - 64 * 1024,
        }
    }
}

/// Something the host of a hand must do.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// Start this call, and report how it ends with [`Core::finished`].
    Start(Call),
    /// Stop this running call, then report it with [`Core::finished`].
    Cancel(CallId),
    /// Send this to the mind.
    Send(FromHand),
}

/// What a hand remembers.
///
/// The core serializes whole, so a host whose memory can vanish keeps it
/// wherever it keeps anything else.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Core {
    mind: Name,
    hand: Name,
    boot: String,
    last_id: CallId,
    running: BTreeSet<CallId>,
    held: VecDeque<Held>,
    held_bytes: usize,
    limits: Limits,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Held {
    result: CallResult,
    bytes: usize,
}

impl Core {
    /// The hand `hand` of the mind `mind`, which has read no call.
    ///
    /// `boot` is random per hand process: a host draws a new one each time it
    /// starts a hand from nothing, and keeps the old one when it restores a
    /// hand with everything it remembered.
    pub fn new(mind: Name, hand: Name, boot: impl Into<String>, limits: Limits) -> Self {
        Self {
            mind,
            hand,
            boot: boot.into(),
            last_id: 0,
            running: BTreeSet::new(),
            held: VecDeque::new(),
            held_bytes: 0,
            limits,
        }
    }

    /// The mind this hand joins.
    pub fn mind(&self) -> &Name {
        &self.mind
    }

    /// The hand's name in its mind.
    pub fn hand(&self) -> &Name {
        &self.hand
    }

    /// The highest call id this hand has read.
    pub fn last_id(&self) -> CallId {
        self.last_id
    }

    /// The calls still running.
    pub fn running(&self) -> impl Iterator<Item = CallId> + '_ {
        self.running.iter().copied()
    }

    /// The first message of a connection.
    pub fn hello(&self, capabilities: Vec<CapabilityDecl>) -> Hello {
        Hello {
            mind: self.mind.clone(),
            hand: self.hand.clone(),
            boot: self.boot.clone(),
            last_id: self.last_id,
            running: self.running.iter().copied().collect(),
            held: self.held.iter().map(|held| held.result.clone()).collect(),
            capabilities,
        }
    }

    /// Takes one message from the mind.
    pub fn receive(&mut self, message: FromMind) -> Vec<Effect> {
        match message {
            FromMind::Call(call) if call.id <= self.last_id => {
                let reason = format!(
                    "call {} is not above the last id read, {}",
                    call.id, self.last_id
                );
                vec![Effect::Send(FromHand::Refused(Refused {
                    id: call.id,
                    reason,
                }))]
            }
            FromMind::Call(call) => {
                self.last_id = call.id;
                self.running.insert(call.id);
                vec![Effect::Start(call)]
            }
            FromMind::Cancel { id } if self.running.contains(&id) => vec![Effect::Cancel(id)],
            FromMind::Cancel { .. } => Vec::new(),
            FromMind::Ack { ids } => {
                self.held.retain(|held| !ids.contains(&held.result.id));
                self.held_bytes = self.held.iter().map(|held| held.bytes).sum();
                Vec::new()
            }
        }
    }

    /// Ends every running call as lost, and returns their ids.
    ///
    /// A core that says it is running calls when a host starts from it was
    /// stored or copied while they ran, and the process that ran them is
    /// gone. What they did before that is not known, so each is held as an
    /// error that says so, for the mind to take at the next `hello`.
    pub fn interrupted(&mut self) -> Vec<CallId> {
        let lost: Vec<CallId> = self.running.iter().copied().collect();
        for id in &lost {
            let error = "the hand was restarted while this call ran: its outcome is unknown, \
                         and it may have run in part";
            self.finished(*id, Reply::Error(error.into()));
        }
        lost
    }

    /// Records how a running call ended, and returns the result to send if a
    /// mind is connected. The core holds the result either way, until the
    /// mind acks it. `None` when the call is not running.
    ///
    /// A result too large to hold is replaced by an error that says so. When
    /// the held results pass [`Limits::max_held_bytes`], the oldest are
    /// forgotten, and the mind settles them as unknown.
    pub fn finished(&mut self, id: CallId, reply: Reply) -> Option<CallResult> {
        if !self.running.remove(&id) {
            return None;
        }
        let mut result = CallResult { id, reply };
        let mut bytes = written(&result);
        if bytes > self.limits.max_held_bytes {
            let error = format!(
                "the result was {bytes} bytes, more than the {} a hand holds",
                self.limits.max_held_bytes
            );
            result = CallResult {
                id,
                reply: Reply::Error(error),
            };
            bytes = written(&result);
        }
        self.held.push_back(Held {
            result: result.clone(),
            bytes,
        });
        self.held_bytes += bytes;
        while self.held_bytes > self.limits.max_held_bytes && self.held.len() > 1 {
            if let Some(oldest) = self.held.pop_front() {
                self.held_bytes -= oldest.bytes;
            }
        }
        Some(result)
    }
}

fn written(result: &CallResult) -> usize {
    FromHand::Result(result.clone()).encode().len()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn name(name: &str) -> Name {
        Name::new(name).unwrap()
    }

    fn core() -> Core {
        Core::new(name("m"), name("a1"), "boot-1", Limits::default())
    }

    fn call(id: CallId) -> Call {
        Call {
            id,
            capability: "commands".into(),
            operation: "bash".into(),
            input: json!({}),
        }
    }

    fn start(core: &mut Core, id: CallId) {
        assert_eq!(
            core.receive(FromMind::Call(call(id))),
            vec![Effect::Start(call(id))]
        );
    }

    #[test]
    fn a_call_runs_once() {
        let mut core = core();
        start(&mut core, 1);
        start(&mut core, 3);
        for stale in [1, 2, 3] {
            let effects = core.receive(FromMind::Call(call(stale)));
            assert!(
                matches!(&effects[..], [Effect::Send(FromHand::Refused(r))] if r.id == stale),
                "{effects:?}"
            );
        }
        assert_eq!(core.last_id(), 3);
        assert_eq!(core.hello(Vec::new()).last_id, 3);
        assert_eq!(core.running().collect::<Vec<_>>(), vec![1, 3]);
    }

    #[test]
    fn a_result_is_held_until_it_is_acked() {
        let mut core = core();
        start(&mut core, 1);
        start(&mut core, 2);
        let first = core.finished(1, Reply::Output(json!("one"))).unwrap();
        let second = core.finished(2, Reply::Error("two".into())).unwrap();

        let hello = core.hello(Vec::new());
        assert_eq!(hello.held, vec![first, second.clone()]);
        assert!(hello.running.is_empty());

        assert!(core.receive(FromMind::Ack { ids: vec![1, 99] }).is_empty());
        assert_eq!(core.hello(Vec::new()).held, vec![second]);
    }

    #[test]
    fn only_a_running_call_finishes_or_is_cancelled() {
        let mut core = core();
        assert_eq!(core.finished(1, Reply::Output(json!(1))), None);
        assert!(core.receive(FromMind::Cancel { id: 1 }).is_empty());

        start(&mut core, 1);
        assert_eq!(
            core.receive(FromMind::Cancel { id: 1 }),
            vec![Effect::Cancel(1)]
        );
        assert!(core.finished(1, Reply::Error("cancelled".into())).is_some());
        assert_eq!(core.finished(1, Reply::Error("cancelled".into())), None);
    }

    #[test]
    fn the_oldest_results_go_when_the_hand_holds_too_much() {
        let mut core = Core::new(
            name("m"),
            name("a1"),
            "boot-1",
            Limits {
                max_held_bytes: 200,
            },
        );
        for id in 1..=4 {
            start(&mut core, id);
            core.finished(id, Reply::Output(json!("x".repeat(40))))
                .unwrap();
        }
        let held: Vec<_> = core.hello(Vec::new()).held.iter().map(|r| r.id).collect();
        assert_eq!(held, vec![3, 4]);
    }

    #[test]
    fn a_result_too_large_to_hold_becomes_an_error() {
        let mut core = Core::new(
            name("m"),
            name("a1"),
            "boot-1",
            Limits {
                max_held_bytes: 200,
            },
        );
        start(&mut core, 1);
        let result = core
            .finished(1, Reply::Output(json!("x".repeat(500))))
            .unwrap();
        assert!(matches!(&result.reply, Reply::Error(e) if e.contains("more than the 200")));
        assert_eq!(core.hello(Vec::new()).held, vec![result]);
    }

    #[test]
    fn a_core_survives_being_stored() {
        let mut core = core();
        start(&mut core, 1);
        start(&mut core, 2);
        core.finished(1, Reply::Output(json!("one"))).unwrap();
        let stored = serde_json::to_string(&core).unwrap();
        assert_eq!(serde_json::from_str::<Core>(&stored).unwrap(), core);
    }
}

use crate::{CallId, Hello, Reply};

/// What becomes of one call the mind still waits on, once a hand says hello.
#[derive(Clone, Debug, PartialEq)]
pub enum Settled {
    /// The hand holds its result: take it.
    Take(Reply),
    /// The hand is still running it: wait for its result.
    Wait,
    /// The hand never read it: send it again under the same id.
    Resend,
    /// The hand read it but holds no result, or is a hand that restarted. It
    /// may have run, so nothing is sent again.
    Unknown,
}

/// What a mind does with a [`Hello`].
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    /// Each call the mind waits on, in the order given, and what becomes of it.
    pub calls: Vec<(CallId, Settled)>,
    /// The results to ack once those taken are stored: every one the hand
    /// holds, including any the mind already had.
    pub ack: Vec<CallId>,
    /// Whether this is a hand process the mind has not recorded: its first,
    /// or one that restarted.
    pub new_boot: bool,
}

/// Settles a connection.
///
/// One rule covers a hand's first connection and every reconnect. `recorded`
/// is the boot the mind last stored for this hand, and `pending` the calls it
/// sent this hand that have no result yet.
///
/// 1. The boot changed: the hand restarted and remembers none of the pending
///    calls. Each may have run, so each is [`Settled::Unknown`].
/// 2. Otherwise, for each pending call, the first that holds:
///    - the hand holds its result: [`Settled::Take`];
///    - the hand is running it: [`Settled::Wait`];
///    - its id is above the hand's `lastId`: [`Settled::Resend`];
///    - otherwise it was read and is gone: [`Settled::Unknown`].
pub fn settle(recorded: Option<&str>, pending: &[CallId], hello: &Hello) -> Plan {
    let new_boot = recorded != Some(hello.boot.as_str());
    let restarted = recorded.is_some() && new_boot;
    let calls = pending
        .iter()
        .map(|&id| {
            let settled = if restarted {
                Settled::Unknown
            } else if let Some(result) = hello.held.iter().find(|result| result.id == id) {
                Settled::Take(result.reply.clone())
            } else if hello.running.contains(&id) {
                Settled::Wait
            } else if id > hello.last_id {
                Settled::Resend
            } else {
                Settled::Unknown
            };
            (id, settled)
        })
        .collect();
    Plan {
        calls,
        ack: hello.held.iter().map(|result| result.id).collect(),
        new_boot,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{CallResult, Name};

    fn hello(boot: &str, last_id: CallId, running: &[CallId], held: &[CallId]) -> Hello {
        Hello {
            mind: Name::new("m").unwrap(),
            hand: Name::new("a1").unwrap(),
            boot: boot.into(),
            last_id,
            running: running.to_vec(),
            held: held
                .iter()
                .map(|&id| CallResult {
                    id,
                    reply: Reply::Output(json!(id)),
                })
                .collect(),
            capabilities: Vec::new(),
        }
    }

    #[test]
    fn a_first_connection_has_nothing_to_settle() {
        let plan = settle(None, &[], &hello("b1", 0, &[], &[]));
        assert_eq!(
            plan,
            Plan {
                calls: Vec::new(),
                ack: Vec::new(),
                new_boot: true
            }
        );
    }

    #[test]
    fn each_pending_call_takes_the_first_row_that_matches() {
        let plan = settle(
            Some("b1"),
            &[38, 39, 40, 41, 42],
            &hello("b1", 41, &[41], &[39, 40]),
        );
        assert_eq!(
            plan.calls,
            vec![
                (38, Settled::Unknown),
                (39, Settled::Take(Reply::Output(json!(39)))),
                (40, Settled::Take(Reply::Output(json!(40)))),
                (41, Settled::Wait),
                (42, Settled::Resend),
            ]
        );
        assert_eq!(plan.ack, vec![39, 40]);
        assert!(!plan.new_boot);
    }

    #[test]
    fn a_hand_that_restarted_knows_no_pending_call() {
        let plan = settle(Some("b1"), &[41, 42], &hello("b2", 0, &[], &[]));
        assert_eq!(
            plan.calls,
            vec![(41, Settled::Unknown), (42, Settled::Unknown)]
        );
        assert!(plan.new_boot);
    }

    #[test]
    fn a_restarted_hands_results_are_acked_but_not_taken() {
        let plan = settle(Some("b1"), &[1], &hello("b2", 1, &[], &[1]));
        assert_eq!(plan.calls, vec![(1, Settled::Unknown)]);
        assert_eq!(plan.ack, vec![1]);
    }

    #[test]
    fn a_result_the_mind_already_has_is_only_acked() {
        let plan = settle(Some("b1"), &[], &hello("b1", 9, &[], &[8, 9]));
        assert!(plan.calls.is_empty());
        assert_eq!(plan.ack, vec![8, 9]);
    }
}

use std::borrow::Cow;
use std::collections::{BTreeMap, VecDeque};

use mensor_protocol::{CapabilityDecl, Name};
use serde_json::Value;

use crate::tools::abilities;
use crate::{Block, Content, HandRecord, Item, Message, Outcome, Role};

/// What stands before a summary when the model is shown one.
const SUMMARISED: &str =
    "This conversation grew too long to keep, and was replaced by this summary of it:";

/// The conversation a history amounts to, as a model reads it.
///
/// Each step is one assistant message. The user message after it carries the
/// results of the tools that step used, then every prompt that arrived before
/// the next step was asked for. A prompt that arrived while a step was being
/// asked for follows that step. A pause is told where it happened, among the
/// prompts, so that the model knows the turn before it was cut short. So is
/// a hand joining the mind, with what it can do, and a hand let go. A
/// message from the mind's parent or one of its children says whose it is.
///
/// A history that holds a summary starts at its last one: the summary, the
/// hands the mind has and the minds it is kin to, then every prompt no step
/// had seen when it was written.
///
/// `items` are the end of a history, and `before` is how many items of it
/// come before them: none, or those a summary replaced. The results of one
/// step are shown in `max_results` bytes at the most, shared equally
/// between them. A step's reasoning is shown only if it was asked `under`
/// the fingerprint of the request this is for.
pub(crate) fn window(items: &[Item], before: u64, max_results: usize, under: &str) -> Vec<Message> {
    conversation(items, before, None, max_results, under)
}

/// The request for a summary: everything the steps have seen, then `ask`.
/// A prompt no step has seen is left out, and follows the summary as it was
/// said.
pub(crate) fn to_summarise(
    items: &[Item],
    before: u64,
    ask: &str,
    max_results: usize,
    under: &str,
) -> Vec<Message> {
    conversation(items, before, Some(ask), max_results, under)
}

fn conversation(
    items: &[Item],
    before: u64,
    ask: Option<&str>,
    max_results: usize,
    under: &str,
) -> Vec<Message> {
    let summary = items
        .iter()
        .enumerate()
        .rev()
        .find_map(|(at, item)| match item {
            Item::Summary {
                text,
                from,
                hands,
                parent,
                children,
                ..
            } => Some((at, text, *from, hands, (parent.as_ref(), children))),
            _ => None,
        });
    // What the next user message holds: results first, then prompts.
    let mut user: Vec<Content> = Vec::new();
    // The hands the model has been told of, with what each can do.
    let mut told = BTreeMap::new();
    // Steps count from the summary. Prompts count from the index it names:
    // what the last step before it had seen.
    let (steps_from, prompts_from) = match summary {
        Some((at, text, from, hands, kin)) => {
            user.extend(summed_up(text, hands, kin));
            told.extend(
                hands
                    .iter()
                    .map(|(name, hand)| (name, hand.capabilities.as_slice())),
            );
            let from = usize::try_from(from.saturating_sub(before)).unwrap_or(usize::MAX);
            (at, from.min(at))
        }
        None => (0, 0),
    };

    let mut messages = Vec::new();
    let mut prompts: VecDeque<(u64, Cow<'_, str>)> = VecDeque::new();
    // The tool asked of the last step, each with its outcome once one is read.
    let mut asked: Vec<(&str, Option<&Outcome>)> = Vec::new();
    for (at, (index, item)) in (before..).zip(items).enumerate().skip(prompts_from) {
        let counted = at >= steps_from;
        if let Some(said) = among_prompts(item, counted, &mut told) {
            prompts.push_back((index, said));
            continue;
        }
        match item {
            Item::Step {
                seen,
                content,
                under: was,
                ..
            } if counted => {
                user.extend(results(&asked, max_results));
                while let Some((_, text)) = prompts.pop_front_if(|(at, _)| *at < *seen) {
                    user.push(Content::Text(text.into_owned()));
                }
                push_user(&mut messages, std::mem::take(&mut user));

                let said = spoken(content, was.as_deref() == Some(under));
                if !said.is_empty() {
                    messages.push(Message {
                        role: Role::Assistant,
                        content: said,
                    });
                }
                asked = content
                    .iter()
                    .filter_map(|block| match block {
                        Block::ToolUse { id, .. } => Some((id.as_str(), None)),
                        Block::Text { .. } | Block::Other { .. } => None,
                    })
                    .collect();
            }
            // An outcome answers a tool use of the step before it.
            Item::Outcome {
                tool_use, outcome, ..
            } if counted => {
                let open = asked
                    .iter_mut()
                    .find(|(id, ended)| *id == tool_use && ended.is_none());
                if let Some((_, ended)) = open {
                    *ended = Some(outcome);
                }
            }
            _ => {}
        }
    }
    user.extend(results(&asked, max_results));
    match ask {
        Some(ask) => user.push(Content::Text(ask.to_owned())),
        None => user.extend(
            prompts
                .into_iter()
                .map(|(_, text)| Content::Text(text.into_owned())),
        ),
    }
    push_user(&mut messages, user);
    messages
}

/// What an item tells the model among the prompts, if anything: a prompt,
/// marked with the mind that said it when one did; a pause; a hand joining
/// for the first time, or able to do something other than `told` records;
/// and, once it `counted`, a hand let go.
///
/// What an item says depends on it and the items before it, never on what
/// came after or who reads it: a child and its parent write the history
/// they share alike, so the child's request starts as its parent's did.
fn among_prompts<'a>(
    item: &'a Item,
    counted: bool,
    told: &mut BTreeMap<&'a Name, &'a [CapabilityDecl]>,
) -> Option<Cow<'a, str>> {
    let note = match item {
        Item::Prompt {
            text, from: None, ..
        } => return Some(Cow::Borrowed(text)),
        Item::Prompt {
            text,
            from: Some(from),
            ..
        } => format!("(From {from}:)\n\n{text}"),
        Item::Paused { reason } => format!("(The turn before this was stopped: {reason}.)"),
        Item::Attached {
            hand, capabilities, ..
        } => match told.insert(hand, capabilities) {
            None => format!("(Hand {hand} joined. It can: {}.)", can(capabilities)),
            Some(before) if before != capabilities.as_slice() => {
                format!("(Hand {hand} can now: {}.)", can(capabilities))
            }
            Some(_) => return None,
        },
        Item::Released { hand, reason } if counted => {
            format!("(Hand {hand} was let go: {reason}.)")
        }
        _ => return None,
    };
    Some(Cow::Owned(note))
}

/// A summary, with the hands and the kin of the mind that wrote it, as they
/// were then: by name, so that a child it is shared with reads it as its
/// writer does.
fn summed_up(
    text: &str,
    hands: &BTreeMap<Name, HandRecord>,
    (parent, children): (Option<&Name>, &Vec<Name>),
) -> Vec<Content> {
    let mut told = vec![Content::Text(format!("{SUMMARISED}\n\n{text}"))];
    let kept: Vec<String> = hands
        .iter()
        .filter(|(_, hand)| !hand.released)
        .map(|(name, hand)| format!("{name} ({})", can(&hand.capabilities)))
        .collect();
    if !kept.is_empty() {
        told.push(Content::Text(format!("(Hands then: {}.)", kept.join("; "))));
    }
    if let Some(parent) = parent {
        told.push(Content::Text(format!("(Parent then: {parent}.)")));
    }
    if !children.is_empty() {
        let children: Vec<&str> = children.iter().map(Name::as_str).collect();
        told.push(Content::Text(format!(
            "(Children then: {}.)",
            children.join(", ")
        )));
    }
    told
}

/// What a hand can do, as the model is told it.
fn can(capabilities: &[CapabilityDecl]) -> String {
    let (operations, clones) = abilities(capabilities);
    let mut can = operations.join(", ");
    if clones {
        can.push_str(if can.is_empty() {
            "be cloned"
        } else {
            ", and be cloned"
        });
    }
    can
}

/// The results of a step's tool asked that have ended, in the step's order,
/// each within its share of `max` bytes.
fn results<'a>(
    asked: &'a [(&str, Option<&Outcome>)],
    max: usize,
) -> impl Iterator<Item = Content> + 'a {
    let share = max / asked.len().max(1);
    asked
        .iter()
        .filter_map(move |(id, outcome)| outcome.map(|outcome| result(id, outcome, share)))
}

fn push_user(messages: &mut Vec<Message>, content: Vec<Content>) {
    if content.is_empty() {
        return;
    }
    match messages.last_mut() {
        Some(Message {
            role: Role::User,
            content: last,
        }) => last.extend(content),
        _ => messages.push(Message {
            role: Role::User,
            content,
        }),
    }
}

/// What a step said, as the model is shown it again. A model's reasoning
/// is bound to the model, system prompt and tools it was made under, so it
/// is shown only to a request made `alike`. Shown to any other, the API
/// would refuse it, or drop it and every block after it; left out, a
/// mind's later reasoning stands on its own.
fn spoken(content: &[Block], alike: bool) -> Vec<Content> {
    content
        .iter()
        .filter(|block| alike || !matches!(block, Block::Other { .. }))
        .map(said)
        .collect()
}

fn said(block: &Block) -> Content {
    match block {
        Block::Text { text } => Content::Text(text.clone()),
        Block::Other { value } => Content::Other(value.clone()),
        Block::ToolUse { id, name, input } => Content::ToolUse {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
        },
    }
}

fn result(tool_use: &str, outcome: &Outcome, max: usize) -> Content {
    let (text, error) = match outcome {
        Outcome::Output(Value::String(text)) => (text.clone(), false),
        Outcome::Output(output) => (output.to_string(), false),
        Outcome::Error(error) => (error.clone(), true),
        Outcome::NotRun(why) => (format!("This call did not run: {why}."), true),
        Outcome::Unknown(why) => (
            format!(
                "The outcome of this call is unknown: {why}. It may have run. \
                 Check the current state before running it again."
            ),
            true,
        ),
    };
    Content::ToolResult {
        tool_use: tool_use.to_owned(),
        text: fitted(text, max),
        error,
    }
}

/// `text` if it is `max` bytes or fewer. Otherwise its two ends, about `max`
/// bytes of them, with a note of how much was left out between.
fn fitted(text: String, max: usize) -> String {
    if text.len() <= max {
        return text;
    }
    let head = text.floor_char_boundary(max / 2);
    let tail = text.ceil_char_boundary(text.len() - (max - max / 2));
    format!(
        "{}\n... {} bytes omitted: this result was too long to show whole ...\n{}",
        &text[..head],
        tail - head,
        &text[tail..]
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{Stop, Usage};

    fn prompt(text: &str) -> Item {
        Item::Prompt {
            key: None,
            text: text.into(),
            from: None,
        }
    }

    fn step(seen: u64, content: Vec<Block>) -> Item {
        let stop = if content
            .iter()
            .any(|block| matches!(block, Block::ToolUse { .. }))
        {
            Stop::ToolUse
        } else {
            Stop::EndTurn
        };
        Item::Step {
            seen,
            content,
            stop,
            usage: Usage::default(),
            model: None,
            under: None,
        }
    }

    fn text(text: &str) -> Block {
        Block::Text { text: text.into() }
    }

    fn tool_use(id: &str) -> Block {
        Block::ToolUse {
            id: id.into(),
            name: "bash".into(),
            input: json!({}),
        }
    }

    fn outcome(tool_use: &str, outcome: Outcome) -> Item {
        Item::Outcome {
            tool_use: tool_use.into(),
            call: None,
            outcome,
        }
    }

    fn user(content: Vec<Content>) -> Message {
        Message {
            role: Role::User,
            content,
        }
    }

    fn assistant(content: Vec<Content>) -> Message {
        Message {
            role: Role::Assistant,
            content,
        }
    }

    fn told(tool_use: &str, text: &str, error: bool) -> Content {
        Content::ToolResult {
            tool_use: tool_use.into(),
            text: text.into(),
            error,
        }
    }

    #[test]
    fn prompts_and_steps_alternate() {
        let items = [prompt("one"), step(1, vec![text("two")]), prompt("three")];
        assert_eq!(
            window(&items, 0, usize::MAX, ""),
            vec![
                user(vec![Content::Text("one".into())]),
                assistant(vec![Content::Text("two".into())]),
                user(vec![Content::Text("three".into())]),
            ]
        );
    }

    #[test]
    fn results_follow_the_step_that_asked_in_its_order() {
        let items = [
            prompt("go"),
            step(1, vec![tool_use("a"), tool_use("b")]),
            outcome("b", Outcome::Error("no".into())),
            outcome("a", Outcome::Output(json!({"exit": 0}))),
        ];
        let messages = window(&items, 0, usize::MAX, "");
        assert_eq!(
            messages[2],
            user(vec![
                told("a", r#"{"exit":0}"#, false),
                told("b", "no", true)
            ])
        );
    }

    #[test]
    fn a_prompt_that_came_while_a_step_was_asked_for_follows_it() {
        // The step was asked for when the history held one item, so the
        // prompt at index 1 was too late for it.
        let items = [
            prompt("first"),
            prompt("late"),
            step(1, vec![text("answer")]),
        ];
        assert_eq!(
            window(&items, 0, usize::MAX, ""),
            vec![
                user(vec![Content::Text("first".into())]),
                assistant(vec![Content::Text("answer".into())]),
                user(vec![Content::Text("late".into())]),
            ]
        );
    }

    #[test]
    fn a_prompt_during_a_round_joins_the_results() {
        let items = [
            prompt("go"),
            step(1, vec![tool_use("a")]),
            prompt("also this"),
            outcome("a", Outcome::Output(json!("done"))),
        ];
        assert_eq!(
            window(&items, 0, usize::MAX, "")[2],
            user(vec![
                told("a", "done", false),
                Content::Text("also this".into())
            ])
        );
    }

    #[test]
    fn outcomes_that_are_not_results_say_what_happened() {
        let items = [
            prompt("go"),
            step(1, vec![tool_use("a"), tool_use("b")]),
            outcome(
                "a",
                Outcome::NotRun("forked before this call started".into()),
            ),
            outcome("b", Outcome::Unknown("the hand restarted".into())),
        ];
        let Content::ToolResult {
            text: not_run,
            error: true,
            ..
        } = &window(&items, 0, usize::MAX, "")[2].content[0]
        else {
            panic!("not an error result");
        };
        assert_eq!(
            not_run,
            "This call did not run: forked before this call started."
        );
        let Content::ToolResult {
            text: unknown,
            error: true,
            ..
        } = &window(&items, 0, usize::MAX, "")[2].content[1]
        else {
            panic!("not an error result");
        };
        assert!(unknown.contains("unknown: the hand restarted") && unknown.contains("Check"));
    }

    #[test]
    fn prompts_with_no_step_between_them_are_one_message() {
        let items = [
            prompt("one"),
            Item::Failed {
                error: "model down".into(),
            },
            prompt("two"),
        ];
        assert_eq!(
            window(&items, 0, usize::MAX, ""),
            vec![user(vec![
                Content::Text("one".into()),
                Content::Text("two".into())
            ])]
        );
    }
}

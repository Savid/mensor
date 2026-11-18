//! The tools a mind shows its model, and what each one does.
//!
//! A hand's capabilities become tools, one per operation, whichever hands
//! offer it, and each takes a `hand` argument that says which hand runs it.
//! Hand names never appear in a definition, so the tools change only when a
//! new kind of capability arrives, and a model's cache of its prompt
//! survives hands coming and going.
//!
//! The mind has tools of its own: `fork`, `clone_hand` and `release_hand`,
//! shown while a hand can clone itself, and `tell` and `wait`, shown while
//! a hand can clone itself or the mind has kin. Forking needs the first, so
//! a fork leaves its tools as they were, and its children start with the
//! same. A host may offer only some of them
//! ([`Settings::tools`](crate::Settings::tools)).

use std::str::FromStr;
use std::time::Duration;

use mensor_protocol::{CapabilityDecl, Name, OperationDecl};
use serde_json::{Map, Value, json};

use crate::Tool;

/// The capability a hand declares when it can clone itself. The model never
/// sees it as a tool: the mind's own tools use it.
pub const CLONE: &str = "clone";

/// The most children one `fork` makes.
pub const MAX_CHILDREN: usize = 8;

/// The argument every hand tool takes.
pub(crate) const HAND: &str = "hand";

/// How many seconds a `wait` lasts when the model does not say.
const WAIT_SECONDS: u64 = 600;

/// The most seconds a `wait` lasts.
const MAX_WAIT_SECONDS: u64 = 3600;

/// One of the mind's own tools.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MindTool {
    /// `fork`: makes child minds, each on a clone of a hand.
    Fork,
    /// `clone_hand`: makes another hand of the mind from one of its own.
    CloneHand,
    /// `release_hand`: lets one of the mind's hands go.
    ReleaseHand,
    /// `tell`: says something to the mind's parent or a child.
    Tell,
    /// `wait`: waits for children to end their turns.
    Wait,
}

/// A name that is not one of the mind's own tools.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0} is not one of a mind's own tools: fork, clone_hand, release_hand, tell or wait")]
pub struct NoMindTool(pub String);

impl MindTool {
    /// Every one, in the order they are shown.
    pub const ALL: [Self; 5] = [
        Self::Fork,
        Self::CloneHand,
        Self::ReleaseHand,
        Self::Tell,
        Self::Wait,
    ];

    /// Its name, as the model is shown it.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Fork => "fork",
            Self::CloneHand => "clone_hand",
            Self::ReleaseHand => "release_hand",
            Self::Tell => "tell",
            Self::Wait => "wait",
        }
    }

    fn tool(self) -> Tool {
        let (description, input) = match self {
            Self::Fork => (
                "Fork this conversation into child minds, one per prompt. Each child is given \
                 this conversation as it is now, a clone of one of your hands (a copy of it, its \
                 files included), and its own prompt, and then works on its own: this call only \
                 starts them. Use wait to read what they answer, and tell to say more to one.",
                json!({
                    "type": "object",
                    "properties": {
                        "hand": {"type": "string", "description":
                            "The hand to clone for each child. You may leave it out when only one of your hands can be cloned."},
                        "prompts": {"type": "array", "items": {"type": "string"},
                            "minItems": 1, "maxItems": MAX_CHILDREN,
                            "description": "One prompt per child: what that child is to do."},
                    },
                    "required": ["prompts"],
                }),
            ),
            Self::CloneHand => (
                "Clone one of your hands. The clone is a copy of it, its files included, and \
                 joins you as another hand, so that you can work in both at once.",
                json!({
                    "type": "object",
                    "properties": {
                        "hand": {"type": "string", "description":
                            "The hand to clone. You may leave it out when only one of your hands can be cloned."},
                        "name": {"type": "string", "description":
                            "A name for the clone. One is chosen when you leave it out."},
                    },
                }),
            ),
            Self::ReleaseHand => (
                "Let one of your hands go. Its process ends, and it cannot be used again.",
                json!({
                    "type": "object",
                    "properties": {
                        "hand": {"type": "string", "description": "The hand to let go."},
                    },
                    "required": ["hand"],
                }),
            ),
            Self::Tell => (
                "Send a message to your parent or to one of your children. It reaches that mind \
                 as a prompt, marked as yours. This does not wait for an answer.",
                json!({
                    "type": "object",
                    "properties": {
                        "mind": {"type": "string", "description": "Your parent's name or a child's."},
                        "text": {"type": "string", "description": "The message."},
                    },
                    "required": ["mind", "text"],
                }),
            ),
            Self::Wait => (
                "Wait until children have ended their turns, and read what each said last. \
                 Once its seconds have passed, a child still going is reported as working, or \
                 as hands_away with the names of its own hands that are not connected: those \
                 are the child's copies, not your hands, and only whoever runs them can bring \
                 them back. Wait again to wait longer.",
                json!({
                    "type": "object",
                    "properties": {
                        "minds": {"type": "array", "items": {"type": "string"}, "description":
                            "The children to wait for. Leave it out to wait for all of them."},
                        "seconds": {"type": "integer", "minimum": 1, "maximum": MAX_WAIT_SECONDS,
                            "description": format!("The most seconds to wait: {WAIT_SECONDS} when you leave it out.")},
                    },
                }),
            ),
        };
        Tool {
            name: self.name().into(),
            description: description.into(),
            input,
        }
    }
}

impl FromStr for MindTool {
    type Err = NoMindTool;

    fn from_str(name: &str) -> Result<Self, NoMindTool> {
        Self::ALL
            .into_iter()
            .find(|tool| tool.name() == name)
            .ok_or_else(|| NoMindTool(name.to_owned()))
    }
}

/// How long a `wait` given `input` lasts.
pub(crate) fn wait_time(input: &Value) -> Result<Duration, String> {
    let seconds = match &input["seconds"] {
        Value::Null => Some(WAIT_SECONDS),
        seconds => seconds
            .as_u64()
            .filter(|seconds| (1..=MAX_WAIT_SECONDS).contains(seconds)),
    };
    let seconds =
        seconds.ok_or_else(|| format!("seconds is a whole number from 1 to {MAX_WAIT_SECONDS}"))?;
    Ok(Duration::from_secs(seconds))
}

/// What a tool does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Does {
    /// An operation of a hand's capability.
    Hand {
        capability: String,
        operation: String,
    },
    /// One of the mind's own.
    Mind(MindTool),
}

/// The tools of a mind's hands, and its own.
#[derive(Clone, Debug, Default)]
pub(crate) struct Table {
    /// Each tool, what it does, and the hands that can do it.
    entries: Vec<(Tool, Does, Vec<Name>)>,
}

impl Table {
    /// The tools of `hands`, the mind's hands that are not released, in the
    /// order of their names, then those of the mind's own that are
    /// `offered`. An operation two hands declare is one tool, defined as the
    /// first of them declares it.
    ///
    /// `tell` and `wait` are shown while a hand can be cloned or the mind
    /// has `kin`, the others while a hand can be cloned: so a fork leaves
    /// the mind's tools as they were, and its children start with the same,
    /// and a prompt's cache, and the reasoning bound to it, hold across it.
    pub(crate) fn new<'a>(
        hands: impl IntoIterator<Item = (&'a Name, &'a [CapabilityDecl])>,
        kin: bool,
        offered: &[MindTool],
    ) -> Self {
        let mut operations: Vec<(&str, &OperationDecl, Vec<Name>)> = Vec::new();
        let mut cloners = Vec::new();
        for (hand, capabilities) in hands {
            for capability in capabilities {
                if capability.name == CLONE {
                    cloners.push(hand.clone());
                    continue;
                }
                for operation in &capability.operations {
                    let known = operations.iter_mut().find(|(of, declared, _)| {
                        *of == capability.name && declared.name == operation.name
                    });
                    match known {
                        Some((_, _, holders)) => holders.push(hand.clone()),
                        None => operations.push((&capability.name, operation, vec![hand.clone()])),
                    }
                }
            }
        }

        let shared = |name: &str| {
            operations
                .iter()
                .filter(|(_, operation, _)| operation.name == name)
                .count()
                > 1
                || MindTool::ALL.iter().any(|tool| tool.name() == name)
        };
        let mut entries: Vec<_> = operations
            .iter()
            .map(|(capability, operation, holders)| {
                let name = if shared(&operation.name) {
                    format!("{capability}_{}", operation.name)
                } else {
                    operation.name.clone()
                };
                let tool = Tool {
                    name,
                    description: operation.description.clone(),
                    input: with_hand(&operation.input),
                };
                let does = Does::Hand {
                    capability: (*capability).to_owned(),
                    operation: operation.name.clone(),
                };
                (tool, does, holders.clone())
            })
            .collect();
        for tool in MindTool::ALL {
            let shown = match tool {
                MindTool::Fork | MindTool::CloneHand | MindTool::ReleaseHand => !cloners.is_empty(),
                MindTool::Tell | MindTool::Wait => kin || !cloners.is_empty(),
            };
            if !shown || !offered.contains(&tool) {
                continue;
            }
            let holders = match tool {
                MindTool::Tell | MindTool::Wait => Vec::new(),
                MindTool::Fork | MindTool::CloneHand | MindTool::ReleaseHand => cloners.clone(),
            };
            entries.push((tool.tool(), Does::Mind(tool), holders));
        }
        Self { entries }
    }

    /// The tools, as the model is shown them.
    pub(crate) fn tools(&self) -> Vec<Tool> {
        self.entries.iter().map(|(tool, ..)| tool.clone()).collect()
    }

    /// What the tool named `name` does, and the hands that can do it: for
    /// the mind's own tools, the hands that can be cloned, and none for
    /// `tell` and `wait`.
    ///
    /// A hand tool is also found by the name it had before hands came or
    /// went: its operation alone, when one operation has it, or
    /// `<capability>_<operation>`.
    pub(crate) fn find(&self, name: &str) -> Option<(&Does, &[Name])> {
        match self.find_all(name).as_slice() {
            [only] => Some(*only),
            _ => None,
        }
    }

    /// The tools `name` may mean, by the names they have now, each with
    /// the hands that can do it.
    pub(crate) fn named(&self, name: &str) -> Vec<(&str, &[Name])> {
        let found = self.find_all(name);
        self.entries
            .iter()
            .filter(|(_, does, holders)| {
                found.iter().any(|(of, by)| {
                    std::ptr::eq(*of, does) && std::ptr::eq(*by, holders.as_slice())
                })
            })
            .map(|(tool, _, holders)| (tool.name.as_str(), holders.as_slice()))
            .collect()
    }

    /// Every tool `name` may mean: the one so named, or else each hand
    /// tool it named before hands came or went, by its operation alone or
    /// as `<capability>_<operation>`.
    pub(crate) fn find_all(&self, name: &str) -> Vec<(&Does, &[Name])> {
        if let Some(exact) = self.entries.iter().find(|(tool, ..)| tool.name == name) {
            return vec![found(exact)];
        }
        self.entries
            .iter()
            .filter(|entry| {
                operation_of(entry).is_some_and(|(capability, operation)| {
                    operation == name
                        || name
                            .strip_prefix(capability)
                            .and_then(|rest| rest.strip_prefix('_'))
                            == Some(operation)
                })
            })
            .map(found)
            .collect()
    }
}

/// What an entry does, and the hands that can do it.
fn found((_, does, holders): &(Tool, Does, Vec<Name>)) -> (&Does, &[Name]) {
    (does, holders.as_slice())
}

/// The capability and operation of a hand tool's entry.
fn operation_of(entry: &(Tool, Does, Vec<Name>)) -> Option<(&str, &str)> {
    match &entry.1 {
        Does::Hand {
            capability,
            operation,
        } => Some((capability, operation)),
        Does::Mind(_) => None,
    }
}

/// `schema` with a `hand` property that names the hand to run it.
fn with_hand(schema: &Value) -> Value {
    let mut schema = schema.clone();
    if let Value::Object(object) = &mut schema {
        let properties = object
            .entry("properties")
            .or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(properties) = properties {
            properties.insert(
                HAND.into(),
                json!({"type": "string", "description":
                    "Which of your hands runs this. You may leave it out when you have one hand."}),
            );
        }
    }
    schema
}

/// The hand a tool use names, if it names one, and the rest of its input.
/// A `hand` that is not a valid name is given back as an error to tell the
/// model.
pub(crate) fn split(input: &Value) -> (Option<Result<Name, String>>, Value) {
    let Value::Object(object) = input else {
        return (None, input.clone());
    };
    let mut rest = object.clone();
    let hand = rest.remove(HAND).map(|hand| match hand {
        Value::String(hand) => Name::new(hand.as_str()).map_err(|_| hand),
        other => Err(other.to_string()),
    });
    (hand, Value::Object(rest))
}

/// The names of a hand's operations, as the model is told them when the
/// hand joins.
pub(crate) fn abilities(capabilities: &[CapabilityDecl]) -> (Vec<&str>, bool) {
    let operations = capabilities
        .iter()
        .filter(|capability| capability.name != CLONE)
        .flat_map(|capability| {
            capability
                .operations
                .iter()
                .map(|operation| operation.name.as_str())
        })
        .collect();
    let clones = capabilities
        .iter()
        .any(|capability| capability.name == CLONE);
    (operations, clones)
}

#[cfg(test)]
mod tests {
    use mensor_protocol::OperationDecl;

    use super::*;

    fn capability(name: &str, operations: &[&str]) -> CapabilityDecl {
        CapabilityDecl {
            name: name.into(),
            operations: operations
                .iter()
                .map(|operation| OperationDecl {
                    name: (*operation).into(),
                    description: format!("{name} {operation}"),
                    input: json!({"type": "object", "properties": {"x": {"type": "string"}}}),
                })
                .collect(),
        }
    }

    fn name(name: &str) -> Name {
        Name::new(name).unwrap()
    }

    fn names(table: &Table) -> Vec<String> {
        table.tools().into_iter().map(|tool| tool.name).collect()
    }

    #[test]
    fn a_tool_is_named_after_its_operation_and_takes_a_hand() {
        let a1 = vec![
            capability("commands", &["bash"]),
            capability("files", &["read"]),
        ];
        let table = Table::new([(&name("a1"), a1.as_slice())], false, &[]);
        assert_eq!(names(&table), ["bash", "read"]);
        let read = &table.tools()[1];
        assert!(read.input["properties"]["x"].is_object());
        assert_eq!(read.input["properties"]["hand"]["type"], "string");
        let (does, holders) = table.find("read").unwrap();
        assert_eq!(
            does,
            &Does::Hand {
                capability: "files".into(),
                operation: "read".into()
            }
        );
        assert_eq!(holders, [name("a1")]);
    }

    #[test]
    fn operations_that_share_a_name_carry_their_capability() {
        let a1 = vec![
            capability("files", &["read", "write"]),
            capability("notes", &["read"]),
        ];
        let table = Table::new([(&name("a1"), a1.as_slice())], false, &[]);
        assert_eq!(names(&table), ["files_read", "write", "notes_read"]);
    }

    #[test]
    fn hands_with_one_operation_share_one_tool() {
        let a1 = vec![capability("commands", &["bash"])];
        let b2 = vec![
            capability("commands", &["bash"]),
            capability("fetch", &["fetch"]),
        ];
        let table = Table::new(
            [(&name("a1"), a1.as_slice()), (&name("b2"), b2.as_slice())],
            false,
            &[],
        );
        assert_eq!(names(&table), ["bash", "fetch"]);
        assert_eq!(table.find("bash").unwrap().1, [name("a1"), name("b2")]);
        assert_eq!(table.find("fetch").unwrap().1, [name("b2")]);
    }

    #[test]
    fn the_minds_own_tools_come_with_a_hand_that_can_clone_and_tell_and_wait_with_kin_too() {
        let plain = vec![capability("commands", &["bash"])];
        let hands = [(&name("a1"), plain.as_slice())];
        assert_eq!(names(&Table::new(hands, false, &MindTool::ALL)), ["bash"]);
        assert_eq!(
            names(&Table::new(hands, true, &MindTool::ALL)),
            ["bash", "tell", "wait"]
        );

        let cloning = vec![
            capability("commands", &["bash"]),
            capability(CLONE, &["clone"]),
        ];
        let fork = vec![capability("tools", &["fork"])];
        let table = Table::new(
            [
                (&name("a1"), cloning.as_slice()),
                (&name("b2"), plain.as_slice()),
                (&name("c3"), fork.as_slice()),
            ],
            false,
            &MindTool::ALL,
        );
        assert_eq!(
            names(&table),
            [
                "bash",
                "tools_fork",
                "fork",
                "clone_hand",
                "release_hand",
                "tell",
                "wait"
            ]
        );
        let (does, cloners) = table.find("fork").unwrap();
        assert_eq!(does, &Does::Mind(MindTool::Fork));
        assert_eq!(cloners, [name("a1")]);
    }

    #[test]
    fn a_host_offers_only_the_minds_own_tools_it_chooses() {
        let cloning = vec![capability(CLONE, &["clone"])];
        let hands = [(&name("a1"), cloning.as_slice())];
        let table = Table::new(hands, false, &[MindTool::Fork, MindTool::Wait]);
        assert_eq!(names(&table), ["fork", "wait"]);
        assert!(table.find("clone_hand").is_none());
        assert!(table.find("release_hand").is_none());
        assert_eq!(table.find("fork").unwrap().1, [name("a1")]);
        assert!(Table::new(hands, false, &[]).tools().is_empty());
    }

    #[test]
    fn a_tool_use_names_its_hand_apart_from_its_input() {
        let (hand, rest) = split(&json!({"hand": "b2", "command": "ls"}));
        assert_eq!(hand, Some(Ok(name("b2"))));
        assert_eq!(rest, json!({"command": "ls"}));
        assert_eq!(split(&json!({"command": "ls"})).0, None);
        assert_eq!(split(&json!({"hand": 7})).0, Some(Err("7".into())));
    }
}

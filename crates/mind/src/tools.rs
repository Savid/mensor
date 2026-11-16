//! The tools a mind shows its model, and what each one does.
//!
//! A hand's capabilities become tools, one per operation, whichever hands
//! offer it, and each takes a `hand` argument that says which hand runs it.
//! Hand names never appear in a definition, so the tools change only when a
//! new kind of capability arrives, and a model's cache of its prompt
//! survives hands coming and going.
//!
//! The mind has three tools of its own, shown while a hand can clone itself:
//! `fork`, `clone_hand` and `release_hand`.

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

/// One of the mind's own tools.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MindTool {
    Fork,
    CloneHand,
    ReleaseHand,
}

impl MindTool {
    const ALL: [Self; 3] = [Self::Fork, Self::CloneHand, Self::ReleaseHand];

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Fork => "fork",
            Self::CloneHand => "clone_hand",
            Self::ReleaseHand => "release_hand",
        }
    }

    fn tool(self) -> Tool {
        let (description, input) = match self {
            Self::Fork => (
                "Fork this conversation into child minds, one per prompt. Each child is given \
                 this conversation as it is now, a clone of one of your hands (a copy of it, its \
                 files included), and its own prompt, and then works on its own. Children report \
                 nothing back to you: this call only starts them.",
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
        };
        Tool {
            name: self.name().into(),
            description: description.into(),
            input,
        }
    }
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
    /// order of their names. An operation two hands declare is one tool,
    /// defined as the first of them declares it.
    pub(crate) fn new<'a>(
        hands: impl IntoIterator<Item = (&'a Name, &'a [CapabilityDecl])>,
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
        if !cloners.is_empty() {
            for tool in MindTool::ALL {
                entries.push((tool.tool(), Does::Mind(tool), cloners.clone()));
            }
        }
        Self { entries }
    }

    /// The tools, as the model is shown them.
    pub(crate) fn tools(&self) -> Vec<Tool> {
        self.entries.iter().map(|(tool, ..)| tool.clone()).collect()
    }

    /// What the tool named `name` does, and the hands that can do it: for
    /// the mind's own tools, the hands that can be cloned.
    pub(crate) fn find(&self, name: &str) -> Option<(&Does, &[Name])> {
        let entry = self.entries.iter().find(|(tool, ..)| tool.name == name);
        entry.map(|(_, does, holders)| (does, holders.as_slice()))
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
        let table = Table::new([(&name("a1"), a1.as_slice())]);
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
        let table = Table::new([(&name("a1"), a1.as_slice())]);
        assert_eq!(names(&table), ["files_read", "write", "notes_read"]);
    }

    #[test]
    fn hands_with_one_operation_share_one_tool() {
        let a1 = vec![capability("commands", &["bash"])];
        let b2 = vec![
            capability("commands", &["bash"]),
            capability("fetch", &["fetch"]),
        ];
        let table = Table::new([(&name("a1"), a1.as_slice()), (&name("b2"), b2.as_slice())]);
        assert_eq!(names(&table), ["bash", "fetch"]);
        assert_eq!(table.find("bash").unwrap().1, [name("a1"), name("b2")]);
        assert_eq!(table.find("fetch").unwrap().1, [name("b2")]);
    }

    #[test]
    fn the_minds_own_tools_come_with_a_hand_that_can_clone() {
        let plain = vec![capability("commands", &["bash"])];
        let table = Table::new([(&name("a1"), plain.as_slice())]);
        assert!(table.find("fork").is_none());

        let cloning = vec![
            capability("commands", &["bash"]),
            capability(CLONE, &["clone"]),
        ];
        let fork = vec![capability("tools", &["fork"])];
        let table = Table::new([
            (&name("a1"), cloning.as_slice()),
            (&name("b2"), plain.as_slice()),
            (&name("c3"), fork.as_slice()),
        ]);
        assert_eq!(
            names(&table),
            ["bash", "tools_fork", "fork", "clone_hand", "release_hand"]
        );
        let (does, cloners) = table.find("fork").unwrap();
        assert_eq!(does, &Does::Mind(MindTool::Fork));
        assert_eq!(cloners, [name("a1")]);
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

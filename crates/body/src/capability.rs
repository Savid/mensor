use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use mensor_protocol::{CapabilityDecl, Reply};
use serde::de::DeserializeOwned;
use serde_json::Value;

/// The future of one call to a capability. Dropping it stops the call.
pub type CallFuture<'a> = Pin<Box<dyn Future<Output = Reply> + Send + 'a>>;

/// Something a body can do.
///
/// A capability is known by its name and its operations, not by where it
/// runs: `files` may be a real disk on one body and rows in a database on
/// another. It decides its own details, such as how much output it keeps.
/// The core around it handles ids, held results and acks.
pub trait Capability: Send + Sync {
    /// The capability's name and operations, as the body's `hello` lists them.
    fn declare(&self) -> CapabilityDecl;

    /// Runs one operation. An operation the capability does not have, or
    /// input it cannot use, is a [`Reply::Error`].
    fn call(&self, operation: &str, input: Value) -> CallFuture<'_>;
}

/// Reads an operation's input into the shape the operation takes, or gives
/// the reply that says what is wrong with it.
pub fn input<T: DeserializeOwned>(operation: &str, input: Value) -> Result<T, Reply> {
    serde_json::from_value(input)
        .map_err(|error| Reply::Error(format!("the input of {operation} is not valid: {error}")))
}

/// The capabilities one body has.
#[derive(Clone, Default)]
pub struct Capabilities {
    all: Vec<(String, Arc<dyn Capability>)>,
}

impl Capabilities {
    /// A body that can do nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a capability. One added later under the same name replaces it.
    #[must_use]
    pub fn with(mut self, capability: impl Capability + 'static) -> Self {
        let name = capability.declare().name;
        self.all.retain(|(known, _)| *known != name);
        self.all.push((name, Arc::new(capability)));
        self
    }

    /// What a `hello` lists.
    pub fn declare(&self) -> Vec<CapabilityDecl> {
        self.all
            .iter()
            .map(|(_, capability)| capability.declare())
            .collect()
    }

    /// The capability of this name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Capability>> {
        self.all
            .iter()
            .find(|(known, _)| known == name)
            .map(|(_, capability)| capability.clone())
    }
}

impl std::fmt::Debug for Capabilities {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.all.iter().map(|(name, _)| name))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use mensor_protocol::OperationDecl;
    use serde_json::json;

    use super::*;

    struct Echo(&'static str);

    impl Capability for Echo {
        fn declare(&self) -> CapabilityDecl {
            CapabilityDecl {
                name: "echo".into(),
                operations: vec![OperationDecl {
                    name: "say".into(),
                    description: self.0.into(),
                    input: json!({"type": "object"}),
                }],
            }
        }

        fn call(&self, _operation: &str, input: Value) -> CallFuture<'_> {
            Box::pin(async move { Reply::Output(input) })
        }
    }

    #[test]
    fn a_later_capability_of_the_same_name_replaces_the_earlier() {
        let capabilities = Capabilities::new().with(Echo("first")).with(Echo("second"));
        let declared = capabilities.declare();
        assert_eq!(declared.len(), 1);
        assert_eq!(declared[0].operations[0].description, "second");
        assert!(capabilities.get("echo").is_some());
        assert!(capabilities.get("files").is_none());
    }
}

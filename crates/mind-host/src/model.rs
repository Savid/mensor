use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Mutex, MutexGuard, PoisonError};

pub use mensor_mind::ModelError;
use mensor_mind::{Block, ModelReply, ModelRequest, Stop, Usage};

/// The future of one call to a model.
pub type ModelFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ModelReply, ModelError>> + Send + 'a>>;

/// Where a model tells the text of a reply while it writes it: how many
/// bytes of the text came before, and the next piece. A piece at 0 starts
/// the reply over, as a model does that had to be asked again.
pub type Wrote = Box<dyn FnMut(u64, &str) + Send>;

/// A language model a mind can ask.
///
/// A model retries what is worth retrying itself. An error it returns ends
/// the mind's turn until the next prompt.
pub trait Model: Send + Sync {
    /// Asks the model once. It tells `wrote` the text of its reply as that
    /// comes, if it can.
    fn call(&self, request: ModelRequest, wrote: Wrote) -> ModelFuture<'_>;

    /// The model asked when a request names none: the host's own, which a
    /// mind may always be made with.
    fn name(&self) -> &str;

    /// The efforts its API takes, or none when it takes any that is not
    /// empty.
    fn efforts(&self) -> Option<&'static [&'static str]> {
        None
    }
}

type Script = Box<dyn FnOnce(&ModelRequest) -> ModelReply + Send>;

/// A model that answers from a script, for tests and for trying a host
/// without a real model. It tells each text of a reply whole.
#[derive(Default)]
pub struct Scripted {
    replies: Mutex<VecDeque<Script>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl Scripted {
    /// A model with nothing to say yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a reply of these blocks. It stops for tools if any block uses one.
    pub fn then(&self, content: Vec<Block>) -> &Self {
        self.then_with(move |_| {
            let uses_tools = content
                .iter()
                .any(|block| matches!(block, Block::ToolUse { .. }));
            ModelReply {
                content,
                stop: if uses_tools {
                    Stop::ToolUse
                } else {
                    Stop::EndTurn
                },
                usage: Usage::default(),
            }
        })
    }

    /// Adds a reply worked out from the request it answers.
    pub fn then_with(
        &self,
        reply: impl FnOnce(&ModelRequest) -> ModelReply + Send + 'static,
    ) -> &Self {
        lock(&self.replies).push_back(Box::new(reply));
        self
    }

    /// Every request made so far, oldest first.
    pub fn requests(&self) -> Vec<ModelRequest> {
        lock(&self.requests).clone()
    }
}

impl Model for Scripted {
    fn name(&self) -> &'static str {
        "scripted"
    }

    fn call(&self, request: ModelRequest, mut wrote: Wrote) -> ModelFuture<'_> {
        let script = lock(&self.replies).pop_front();
        let reply = match script {
            Some(script) => Ok(script(&request)),
            None => Err(ModelError("the script has no reply left".into())),
        };
        let mut at = 0;
        for block in reply.iter().flat_map(|reply| &reply.content) {
            if let Block::Text { text } = block {
                wrote(at, text);
                at += u64::try_from(text.len()).unwrap_or(u64::MAX);
            }
        }
        lock(&self.requests).push(request);
        Box::pin(async move { reply })
    }
}

impl std::fmt::Debug for Scripted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scripted")
            .field("replies", &lock(&self.replies).len())
            .field("requests", &lock(&self.requests).len())
            .finish()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

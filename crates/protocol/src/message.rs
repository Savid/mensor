use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Name;

/// The version of the protocol this crate speaks. A [`Hello`] carries it.
pub const VERSION: u32 = 1;

/// The largest message either end accepts, in bytes.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// The id of a call. A mind numbers its calls from 1 and never reuses one.
pub type CallId = u64;

/// A message a hand sends.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromHand {
    /// The first message on every connection.
    Hello(Box<Hello>),
    /// A call finished.
    Result(CallResult),
    /// A call the hand would not start.
    Refused(Refused),
}

/// A message a mind sends. It sends none until it has settled the [`Hello`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromMind {
    /// Start a call.
    Call(Call),
    /// Stop a running call. Its result follows.
    Cancel {
        /// The call to stop.
        id: CallId,
    },
    /// These results are stored: the hand may forget them.
    Ack {
        /// The ids of the stored results.
        ids: Vec<CallId>,
    },
}

/// What a hand says about itself when it connects.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hello {
    /// The protocol version the hand speaks.
    pub v: u32,
    /// The hand's name. One the mind's host has not seen is a new hand.
    pub name: Name,
    /// The name this hand had before it was cloned and fed a new one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub was: Option<Name>,
    /// Random per hand process. A clone made with its memory keeps it; a hand
    /// that restarted has a new one, and remembers nothing of earlier calls.
    pub boot: String,
    /// The highest call id this hand has read, or 0.
    pub last_id: CallId,
    /// The calls still running.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub running: Vec<CallId>,
    /// The results not yet acked, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub held: Vec<CallResult>,
    /// What this hand can do.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<CapabilityDecl>,
}

/// A request for a hand to do one thing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Call {
    /// Above every id the hand has read.
    pub id: CallId,
    /// The capability asked, by name.
    pub capability: String,
    /// The operation of that capability.
    pub operation: String,
    /// The operation's input, shaped as its declared schema says.
    pub input: Value,
}

/// How a call ended on the hand.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CallResult {
    /// The call this answers.
    pub id: CallId,
    /// What the capability returned.
    #[serde(flatten)]
    pub reply: Reply,
}

/// What a capability returned for one call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    /// The operation ran and gave this.
    Output(Value),
    /// The operation failed, or the hand could not run it.
    Error(String),
}

/// A call a hand would not start.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Refused {
    /// The refused call.
    pub id: CallId,
    /// Why, for a log.
    pub reason: String,
}

/// One thing a hand can do, as its [`Hello`] declares it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CapabilityDecl {
    /// The capability's name, such as `commands` or `files`.
    pub name: String,
    /// The operations it offers.
    pub operations: Vec<OperationDecl>,
}

/// One operation of a capability.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OperationDecl {
    /// The operation's name, such as `bash` or `read`.
    pub name: String,
    /// What it does, written for the model that will choose it.
    pub description: String,
    /// The JSON Schema of its input.
    pub input: Value,
}

/// A message that could not be read.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    /// The message is longer than [`MAX_FRAME_BYTES`].
    #[error("a message of {0} bytes is longer than the {MAX_FRAME_BYTES} allowed")]
    TooLong(usize),
    /// The message is not the JSON the protocol defines.
    #[error("not a message of the protocol: {0}")]
    Json(#[from] serde_json::Error),
}

fn decode<'a, T: Deserialize<'a>>(text: &'a str) -> Result<T, DecodeError> {
    if text.len() > MAX_FRAME_BYTES {
        return Err(DecodeError::TooLong(text.len()));
    }
    Ok(serde_json::from_str(text)?)
}

fn encode<T: Serialize>(message: &T) -> String {
    serde_json::to_string(message).expect("a message of the protocol is always JSON")
}

impl FromHand {
    /// Reads one message.
    pub fn decode(text: &str) -> Result<Self, DecodeError> {
        decode(text)
    }

    /// Writes this message as one line of JSON.
    pub fn encode(&self) -> String {
        encode(self)
    }
}

impl FromMind {
    /// Reads one message.
    pub fn decode(text: &str) -> Result<Self, DecodeError> {
        decode(text)
    }

    /// Writes this message as one line of JSON.
    pub fn encode(&self) -> String {
        encode(self)
    }
}

/// The protocol version a `hello` claims, read without the rest of it, so a
/// host can refuse another version before it reads fields that may have
/// changed. `None` when `text` is not a `hello` with a version.
pub fn version_of(text: &str) -> Option<u32> {
    #[derive(Deserialize)]
    struct Peek {
        r#type: String,
        v: u32,
    }
    let peek: Peek = serde_json::from_str(text).ok()?;
    (peek.r#type == "hello").then_some(peek.v)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn name(s: &str) -> Name {
        Name::new(s).unwrap()
    }

    #[test]
    fn a_hello_reads_as_it_is_written() {
        let text = r#"{"type":"hello","v":1,"name":"b7","was":"a1","boot":"9f3c","lastId":41}"#;
        let FromHand::Hello(hello) = FromHand::decode(text).unwrap() else {
            panic!("not a hello");
        };
        assert_eq!(hello.name, name("b7"));
        assert_eq!(hello.was, Some(name("a1")));
        assert_eq!(hello.last_id, 41);
        assert!(hello.running.is_empty() && hello.held.is_empty());
        assert_eq!(FromHand::Hello(hello).encode(), text);
    }

    #[test]
    fn a_result_carries_an_output_or_an_error() {
        let done = FromHand::Result(CallResult {
            id: 7,
            reply: Reply::Output(json!({"exit": 0})),
        });
        assert_eq!(
            done.encode(),
            r#"{"type":"result","id":7,"output":{"exit":0}}"#
        );
        assert_eq!(FromHand::decode(&done.encode()).unwrap(), done);

        let failed = FromHand::Result(CallResult {
            id: 8,
            reply: Reply::Error("spawn".into()),
        });
        assert_eq!(
            failed.encode(),
            r#"{"type":"result","id":8,"error":"spawn"}"#
        );
        assert_eq!(FromHand::decode(&failed.encode()).unwrap(), failed);
    }

    #[test]
    fn a_minds_messages_round_trip() {
        let call = FromMind::Call(Call {
            id: 42,
            capability: "commands".into(),
            operation: "bash".into(),
            input: json!({"command": "cargo test"}),
        });
        assert_eq!(
            call.encode(),
            r#"{"type":"call","id":42,"capability":"commands","operation":"bash","input":{"command":"cargo test"}}"#
        );
        for message in [
            call,
            FromMind::Cancel { id: 42 },
            FromMind::Ack { ids: vec![40, 41] },
        ] {
            assert_eq!(FromMind::decode(&message.encode()).unwrap(), message);
        }
    }

    #[test]
    fn an_unknown_field_is_ignored() {
        let text = r#"{"type":"ack","ids":[1],"later":"field"}"#;
        assert_eq!(
            FromMind::decode(text).unwrap(),
            FromMind::Ack { ids: vec![1] }
        );
    }

    #[test]
    fn a_version_is_read_before_the_rest() {
        assert_eq!(
            version_of(r#"{"type":"hello","v":9,"shape":"changed"}"#),
            Some(9)
        );
        assert_eq!(version_of(r#"{"type":"result","v":9}"#), None);
        assert_eq!(version_of("nonsense"), None);
    }

    #[test]
    fn a_bad_name_refuses_the_hello() {
        let text = r#"{"type":"hello","v":1,"name":"a/b","boot":"x","lastId":0}"#;
        assert!(matches!(FromHand::decode(text), Err(DecodeError::Json(_))));
    }
}

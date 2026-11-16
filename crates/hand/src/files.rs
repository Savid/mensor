use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;

use mensor_protocol::{CapabilityDecl, OperationDecl, Reply};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{CallFuture, Capability};

/// The future of one step on a [`FileSystem`]. Its error is the reason
/// alone: [`Files`] names the path.
pub type FsFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;

/// Where a hand's files are kept: a disk, rows in a database, anything that
/// can hold bytes under a path.
pub trait FileSystem: Send + Sync {
    /// The whole content of the file at `path`.
    fn read(&self, path: &str) -> FsFuture<'_, Vec<u8>>;

    /// Puts `content` at `path`, replacing what was there and making any
    /// directory the path needs.
    fn write(&self, path: &str, content: Vec<u8>) -> FsFuture<'_, ()>;

    /// The names directly under the directory at `path`, each directory's
    /// name ending in `/`.
    fn list(&self, path: &str) -> FsFuture<'_, Vec<String>>;
}

/// The `files` capability: reads and changes files, wherever a
/// [`FileSystem`] keeps them.
///
/// Its operations are `read`, `write`, `edit` and `list`, the same on every
/// hand, so a model uses a file in a database as it uses one on a disk.
#[derive(Clone, Debug)]
pub struct Files<F> {
    files: F,
    /// The most bytes one `read` returns.
    pub max_read: usize,
}

impl<F> Files<F> {
    /// The capability over these files.
    pub fn new(files: F) -> Self {
        Self {
            files,
            max_read: 256 * 1024,
        }
    }
}

#[derive(Deserialize)]
struct Read {
    path: String,
    /// The first line to return, counting from 1.
    offset: Option<usize>,
    /// How many lines to return.
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct Write {
    path: String,
    content: String,
}

#[derive(Deserialize)]
struct Edit {
    path: String,
    old: String,
    new: String,
    #[serde(default)]
    all: bool,
}

#[derive(Deserialize)]
struct List {
    #[serde(default)]
    path: String,
}

impl<F: FileSystem> Capability for Files<F> {
    fn declare(&self) -> CapabilityDecl {
        let path = json!({"type": "string", "description": "The file's path."});
        CapabilityDecl {
            name: "files".into(),
            operations: vec![
                OperationDecl {
                    name: "read".into(),
                    description: "Read a text file, or some of its lines.".into(),
                    input: json!({
                        "type": "object",
                        "properties": {
                            "path": path,
                            "offset": {"type": "integer", "description": "The first line to return, counting from 1."},
                            "limit": {"type": "integer", "description": "How many lines to return."},
                        },
                        "required": ["path"],
                    }),
                },
                OperationDecl {
                    name: "write".into(),
                    description: "Write a file, replacing it if it exists. Missing directories are made.".into(),
                    input: json!({
                        "type": "object",
                        "properties": {
                            "path": path,
                            "content": {"type": "string", "description": "The file's whole content."},
                        },
                        "required": ["path", "content"],
                    }),
                },
                OperationDecl {
                    name: "edit".into(),
                    description: "Replace text in a file. The old text must appear exactly once, unless all is set.".into(),
                    input: json!({
                        "type": "object",
                        "properties": {
                            "path": path,
                            "old": {"type": "string", "description": "The text to replace."},
                            "new": {"type": "string", "description": "The text to put in its place."},
                            "all": {"type": "boolean", "description": "Replace every occurrence."},
                        },
                        "required": ["path", "old", "new"],
                    }),
                },
                OperationDecl {
                    name: "list".into(),
                    description: "List what is directly in a directory. A directory's name ends in /.".into(),
                    input: json!({
                        "type": "object",
                        "properties": {
                            "path": {"type": "string", "description": "The directory's path. Left out, the directory files start from."},
                        },
                    }),
                },
            ],
        }
    }

    fn call(&self, operation: &str, input: Value) -> CallFuture<'_> {
        let operation = operation.to_owned();
        Box::pin(async move {
            let done = match operation.as_str() {
                "read" => match crate::input(&operation, input) {
                    Ok(read) => self.read(read).await,
                    Err(reply) => return reply,
                },
                "write" => match crate::input(&operation, input) {
                    Ok(write) => self.write(write).await,
                    Err(reply) => return reply,
                },
                "edit" => match crate::input(&operation, input) {
                    Ok(edit) => self.edit(edit).await,
                    Err(reply) => return reply,
                },
                "list" => match crate::input(&operation, input) {
                    Ok(list) => self.list(list).await,
                    Err(reply) => return reply,
                },
                _ => Err(format!("files has no operation named {operation}")),
            };
            match done {
                Ok(text) => Reply::Output(Value::String(text)),
                Err(error) => Reply::Error(error),
            }
        })
    }
}

impl<F: FileSystem> Files<F> {
    async fn read(&self, read: Read) -> Result<String, String> {
        let bytes = self
            .files
            .read(&read.path)
            .await
            .map_err(|error| format!("{} could not be read: {error}", read.path))?;
        let text = String::from_utf8_lossy(&bytes);
        let mut text = if read.offset.is_some() || read.limit.is_some() {
            let skip = read.offset.unwrap_or(1).saturating_sub(1);
            let take = read.limit.unwrap_or(usize::MAX);
            text.split_inclusive('\n').skip(skip).take(take).collect()
        } else {
            text.into_owned()
        };
        if text.len() > self.max_read {
            let mut end = self.max_read;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            let omitted = text.len() - end;
            text.truncate(end);
            let _ = write!(
                text,
                "\n... {omitted} bytes omitted; read on with offset and limit ..."
            );
        }
        Ok(text)
    }

    async fn write(&self, write: Write) -> Result<String, String> {
        let bytes = write.content.len();
        self.files
            .write(&write.path, write.content.into_bytes())
            .await
            .map_err(|error| format!("{} could not be written: {error}", write.path))?;
        Ok(format!("wrote {bytes} bytes to {}", write.path))
    }

    async fn edit(&self, edit: Edit) -> Result<String, String> {
        if edit.old.is_empty() {
            return Err("the old text cannot be empty".into());
        }
        let bytes = self
            .files
            .read(&edit.path)
            .await
            .map_err(|error| format!("{} could not be read: {error}", edit.path))?;
        let text = String::from_utf8(bytes).map_err(|_| format!("{} is not text", edit.path))?;
        let found = text.matches(&edit.old).count();
        match found {
            0 => return Err(format!("the old text is not in {}", edit.path)),
            1 => {}
            _ if edit.all => {}
            _ => {
                return Err(format!(
                    "the old text appears {found} times in {}: give more of it, or set all",
                    edit.path
                ));
            }
        }
        self.files
            .write(&edit.path, text.replace(&edit.old, &edit.new).into_bytes())
            .await
            .map_err(|error| format!("{} could not be written: {error}", edit.path))?;
        Ok(format!(
            "replaced {found} occurrence{} in {}",
            if found == 1 { "" } else { "s" },
            edit.path
        ))
    }

    async fn list(&self, list: List) -> Result<String, String> {
        let shown = if list.path.is_empty() {
            "the directory"
        } else {
            &list.path
        };
        let mut names = self
            .files
            .list(&list.path)
            .await
            .map_err(|error| format!("{shown} could not be listed: {error}"))?;
        if names.is_empty() {
            return Ok(format!("{shown} is empty"));
        }
        names.sort();
        Ok(names.join("\n"))
    }
}

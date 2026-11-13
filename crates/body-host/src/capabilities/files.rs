use std::path::{Path, PathBuf};

use mensor_body::{FileSystem, FsFuture};

/// A body's files on its own machine's disk, for the `files` capability.
///
/// A relative path is taken from the body's directory. Nothing confines a
/// path to that directory: keeping a body inside bounds is the job of
/// whoever runs it.
#[derive(Clone, Debug)]
pub struct Disk {
    directory: PathBuf,
}

impl Disk {
    /// Files whose relative paths start at `directory`.
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    fn path(&self, path: &str) -> PathBuf {
        self.directory.join(Path::new(path))
    }
}

impl FileSystem for Disk {
    fn read(&self, path: &str) -> FsFuture<'_, Vec<u8>> {
        let path = self.path(path);
        Box::pin(async move { tokio::fs::read(path).await.map_err(said) })
    }

    fn write(&self, path: &str, content: Vec<u8>) -> FsFuture<'_, ()> {
        let path = self.path(path);
        Box::pin(async move {
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(said)?;
            }
            tokio::fs::write(&path, content).await.map_err(said)
        })
    }

    fn list(&self, path: &str) -> FsFuture<'_, Vec<String>> {
        let path = self.path(path);
        Box::pin(async move {
            let mut entries = tokio::fs::read_dir(path).await.map_err(said)?;
            let mut names = Vec::new();
            while let Some(entry) = entries.next_entry().await.map_err(said)? {
                let mut name = entry.file_name().to_string_lossy().into_owned();
                if entry.file_type().await.is_ok_and(|kind| kind.is_dir()) {
                    name.push('/');
                }
                names.push(name);
            }
            Ok(names)
        })
    }
}

fn said(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use mensor_body::{Capability, Files};
    use mensor_protocol::Reply;
    use serde_json::{Value, json};

    use super::*;

    fn here() -> (tempfile::TempDir, Files<Disk>) {
        let dir = tempfile::tempdir().unwrap();
        let files = Files::new(Disk::new(dir.path()));
        (dir, files)
    }

    async fn ok(files: &Files<Disk>, operation: &str, input: Value) -> String {
        match files.call(operation, input).await {
            Reply::Output(Value::String(text)) => text,
            other => panic!("{operation} did not succeed: {other:?}"),
        }
    }

    async fn error(files: &Files<Disk>, operation: &str, input: Value) -> String {
        match files.call(operation, input).await {
            Reply::Error(error) => error,
            other @ Reply::Output(_) => panic!("{operation} did not fail: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_file_written_reads_back() {
        let (dir, files) = here();
        let wrote = ok(
            &files,
            "write",
            json!({"path": "a/b.txt", "content": "one\ntwo\nthree\n"}),
        )
        .await;
        assert_eq!(wrote, "wrote 14 bytes to a/b.txt");
        assert!(dir.path().join("a/b.txt").exists());
        assert_eq!(
            ok(&files, "read", json!({"path": "a/b.txt"})).await,
            "one\ntwo\nthree\n"
        );
        assert_eq!(
            ok(
                &files,
                "read",
                json!({"path": "a/b.txt", "offset": 2, "limit": 1})
            )
            .await,
            "two\n"
        );
    }

    #[tokio::test]
    async fn a_long_read_says_what_it_left_out() {
        let (_dir, mut files) = here();
        files.max_read = 5;
        ok(
            &files,
            "write",
            json!({"path": "long", "content": "0123456789"}),
        )
        .await;
        let text = ok(&files, "read", json!({"path": "long"})).await;
        assert!(text.starts_with("01234\n... 5 bytes omitted"), "{text}");
    }

    #[tokio::test]
    async fn an_edit_replaces_text_that_appears_once() {
        let (_dir, files) = here();
        ok(&files, "write", json!({"path": "f", "content": "a b a"})).await;
        assert_eq!(
            ok(&files, "edit", json!({"path": "f", "old": "b", "new": "c"})).await,
            "replaced 1 occurrence in f"
        );

        let twice = error(&files, "edit", json!({"path": "f", "old": "a", "new": "z"})).await;
        assert!(twice.contains("appears 2 times"), "{twice}");
        assert_eq!(
            ok(
                &files,
                "edit",
                json!({"path": "f", "old": "a", "new": "z", "all": true})
            )
            .await,
            "replaced 2 occurrences in f"
        );
        assert_eq!(ok(&files, "read", json!({"path": "f"})).await, "z c z");

        let missing = error(&files, "edit", json!({"path": "f", "old": "q", "new": "z"})).await;
        assert!(missing.contains("is not in f"), "{missing}");
    }

    #[tokio::test]
    async fn a_directory_lists_what_is_directly_in_it() {
        let (_dir, files) = here();
        assert_eq!(
            ok(&files, "list", json!({})).await,
            "the directory is empty"
        );
        for path in ["b.txt", "a/one.txt", "a/deep/two.txt"] {
            ok(&files, "write", json!({"path": path, "content": ""})).await;
        }
        assert_eq!(ok(&files, "list", json!({})).await, "a/\nb.txt");
        assert_eq!(
            ok(&files, "list", json!({"path": "a"})).await,
            "deep/\none.txt"
        );
        let missing = error(&files, "list", json!({"path": "nope"})).await;
        assert!(missing.contains("nope could not be listed"), "{missing}");
    }

    #[tokio::test]
    async fn what_cannot_be_done_is_an_error() {
        let (_dir, files) = here();
        assert!(
            error(&files, "read", json!({"path": "nope"}))
                .await
                .contains("could not be read")
        );
        assert!(error(&files, "read", json!({})).await.contains("not valid"));
        assert!(
            error(&files, "delete", json!({}))
                .await
                .contains("no operation named delete")
        );
    }
}

//! The `clone` capability: a hand that copies its own directory and starts a
//! hand on each copy.
//!
//! A mind asks for it through its own tools, `fork` and `clone_hand`, once
//! no call runs anywhere, so the directory is copied at rest. A process a
//! finished command left running in the background is not copied: only the
//! files are.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mensor_hand::{CallFuture, Capability};
use mensor_protocol::{CapabilityDecl, Name, OperationDecl, Reply};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::BoxFuture;

/// Starts a hand. A host that can start hands gives one to [`Cloning`].
pub trait Spawn: Send + Sync {
    /// Starts the hand `hand` of the mind `mind`, working in `dir`.
    fn spawn(
        &self,
        mind: Name,
        hand: Name,
        dir: PathBuf,
    ) -> BoxFuture<'_, Result<Box<dyn Started>, String>>;
}

/// A hand a [`Spawn`] started.
pub trait Started: Send {
    /// Stops it, because the clone that started it was undone.
    fn stop(self: Box<Self>);
}

/// The `clone` capability of a hand that works in a directory.
pub struct Cloning {
    directory: PathBuf,
    clones: PathBuf,
    spawn: Arc<dyn Spawn>,
}

impl Cloning {
    /// Clones of a hand that works in `directory`: each copy is made at
    /// `<clones>/<mind>/<hand>` and started through `spawn`.
    pub fn new(
        directory: impl Into<PathBuf>,
        clones: impl Into<PathBuf>,
        spawn: Arc<dyn Spawn>,
    ) -> Self {
        Self {
            directory: directory.into(),
            clones: clones.into(),
            spawn,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    into: Vec<Target>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Target {
    mind: Name,
    hand: Name,
}

impl Capability for Cloning {
    fn declare(&self) -> CapabilityDecl {
        CapabilityDecl {
            name: "clone".into(),
            operations: vec![OperationDecl {
                name: "clone".into(),
                description: "Copy this hand's directory once for each target, and start a hand \
                              on each copy that joins the target's mind. Every copy is made, or \
                              none."
                    .into(),
                input: json!({
                    "type": "object",
                    "properties": {
                        "into": {"type": "array", "items": {
                            "type": "object",
                            "properties": {"mind": {"type": "string"}, "hand": {"type": "string"}},
                            "required": ["mind", "hand"],
                        }},
                    },
                    "required": ["into"],
                }),
            }],
        }
    }

    fn call(&self, operation: &str, input: Value) -> CallFuture<'_> {
        let input = if operation == "clone" {
            mensor_hand::input::<Input>(operation, input)
        } else {
            Err(Reply::Error(format!("clone has no operation {operation}")))
        };
        Box::pin(async move {
            match input {
                Ok(Input { into }) => self.clone_into(into).await,
                Err(reply) => reply,
            }
        })
    }
}

impl Cloning {
    async fn clone_into(&self, into: Vec<Target>) -> Reply {
        // Undone if this future is dropped or fails before the end.
        let mut made = Made::default();
        let mut clones = Vec::new();
        for Target { mind, hand } in into {
            let dir = self.clones.join(mind.as_str()).join(hand.as_str());
            if dir.exists() {
                return Reply::Error(format!("{} already exists", dir.display()));
            }
            made.dirs.push(dir.clone());
            let (from, to, skip) = (self.directory.clone(), dir.clone(), self.clones.clone());
            let copied = tokio::task::spawn_blocking(move || copy_tree(&from, &to, &skip)).await;
            match copied {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    return Reply::Error(format!("the directory could not be copied: {error}"));
                }
                Err(error) => return Reply::Error(format!("the copy stopped: {error}")),
            }
            match self
                .spawn
                .spawn(mind.clone(), hand.clone(), dir.clone())
                .await
            {
                Ok(started) => made.started.push(started),
                Err(error) => {
                    return Reply::Error(format!("the clone could not be started: {error}"));
                }
            }
            clones.push(json!({"mind": mind, "hand": hand, "dir": dir.display().to_string()}));
        }
        made.keep();
        Reply::Output(json!({ "clones": clones }))
    }
}

/// What a clone has made so far. Dropped before it is kept, it undoes all
/// of it, so a clone makes every copy or none, failed or cancelled.
#[derive(Default)]
struct Made {
    dirs: Vec<PathBuf>,
    started: Vec<Box<dyn Started>>,
    kept: bool,
}

impl Made {
    fn keep(mut self) {
        self.kept = true;
    }
}

impl Drop for Made {
    fn drop(&mut self) {
        if self.kept {
            return;
        }
        for started in self.started.drain(..) {
            started.stop();
        }
        for dir in &self.dirs {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Copies the tree at `from` to `to`, which must not exist, leaving out
/// `skip` if it is inside it. Links are copied as links.
fn copy_tree(from: &Path, to: &Path, skip: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(skip)?;
    let skip = std::fs::canonicalize(skip)?;
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    copy_dir(from, to, &skip)
}

fn copy_dir(from: &Path, to: &Path, skip: &Path) -> std::io::Result<()> {
    std::fs::create_dir(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let (source, target) = (entry.path(), to.join(entry.file_name()));
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(&source)?, &target)?;
        } else if kind.is_dir() {
            if std::fs::canonicalize(&source)? != skip {
                copy_dir(&source, &target, skip)?;
            }
        } else {
            std::fs::copy(&source, &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Starts nothing, but writes down what it is asked, and fails the
    /// start named `fails`.
    #[derive(Default)]
    struct Spawner {
        started: Arc<Mutex<Vec<String>>>,
        stopped: Arc<Mutex<Vec<String>>>,
        fails: Option<&'static str>,
    }

    struct Fake(String, Arc<Mutex<Vec<String>>>);

    impl Started for Fake {
        fn stop(self: Box<Self>) {
            self.1.lock().unwrap().push(self.0);
        }
    }

    impl Spawn for Spawner {
        fn spawn(
            &self,
            mind: Name,
            hand: Name,
            dir: PathBuf,
        ) -> BoxFuture<'_, Result<Box<dyn Started>, String>> {
            Box::pin(async move {
                assert!(dir.join("notes.txt").exists(), "started before the copy");
                if self.fails == Some(mind.as_str()) {
                    return Err("no room".into());
                }
                let started = format!("{mind}/{hand}");
                self.started.lock().unwrap().push(started.clone());
                Ok(Box::new(Fake(started, Arc::clone(&self.stopped))) as Box<dyn Started>)
            })
        }
    }

    fn hand(spawner: Spawner) -> (tempfile::TempDir, Cloning, Arc<Spawner>) {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("work");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("notes.txt"), "hi").unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}").unwrap();
        std::os::unix::fs::symlink("notes.txt", dir.join("link")).unwrap();
        let spawner = Arc::new(spawner);
        let cloning = Cloning::new(&dir, root.path().join("clones"), spawner.clone());
        (root, cloning, spawner)
    }

    fn into(targets: &[(&str, &str)]) -> Value {
        let into: Vec<Value> = targets
            .iter()
            .map(|(mind, hand)| json!({"mind": mind, "hand": hand}))
            .collect();
        json!({ "into": into })
    }

    #[tokio::test]
    async fn each_target_gets_a_copy_with_a_hand_on_it() {
        let (root, cloning, spawner) = hand(Spawner::default());
        let reply = cloning
            .call("clone", into(&[("m.a", "a1"), ("m.b", "a1")]))
            .await;
        let Reply::Output(output) = reply else {
            panic!("{reply:?}");
        };
        assert_eq!(output["clones"].as_array().unwrap().len(), 2);
        let copy = root.path().join("clones/m.b/a1");
        assert_eq!(
            std::fs::read_to_string(copy.join("src/main.rs")).unwrap(),
            "fn main() {}"
        );
        assert_eq!(
            std::fs::read_link(copy.join("link")).unwrap(),
            Path::new("notes.txt")
        );
        assert_eq!(*spawner.started.lock().unwrap(), ["m.a/a1", "m.b/a1"]);
    }

    #[tokio::test]
    async fn a_clone_that_fails_part_way_undoes_what_it_made() {
        let (root, cloning, spawner) = hand(Spawner {
            fails: Some("m.b"),
            ..Spawner::default()
        });
        let reply = cloning
            .call("clone", into(&[("m.a", "a1"), ("m.b", "a1")]))
            .await;
        assert!(
            matches!(&reply, Reply::Error(error) if error.contains("no room")),
            "{reply:?}"
        );
        assert_eq!(*spawner.stopped.lock().unwrap(), ["m.a/a1"]);
        assert!(!root.path().join("clones/m.a/a1").exists());
        assert!(!root.path().join("clones/m.b/a1").exists());
    }

    #[tokio::test]
    async fn a_clone_into_a_copy_that_exists_is_refused() {
        let (root, cloning, _) = hand(Spawner::default());
        std::fs::create_dir_all(root.path().join("clones/m.a/a1")).unwrap();
        let reply = cloning.call("clone", into(&[("m.a", "a1")])).await;
        assert!(matches!(&reply, Reply::Error(error) if error.contains("already exists")));
        assert!(
            root.path().join("clones/m.a/a1").exists(),
            "what was there is kept"
        );
    }

    #[tokio::test]
    async fn clones_kept_inside_the_directory_are_not_copied_into_clones() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("notes.txt"), "hi").unwrap();
        let spawner = Arc::new(Spawner::default());
        let cloning = Cloning::new(root.path(), root.path().join(".clones"), spawner);
        let reply = cloning.call("clone", into(&[("m.a", "a1")])).await;
        assert!(matches!(reply, Reply::Output(_)), "{reply:?}");
        assert!(!root.path().join(".clones/m.a/a1/.clones").exists());
    }
}

//! The `clone` capability: a hand that copies its own directory and starts a
//! hand on each copy.
//!
//! A mind asks for it through its own tools, `fork` and `clone_hand`, once
//! no call runs anywhere, so the directory is copied at rest. A process a
//! finished command left running in the background is not copied: only the
//! files are.
//!
//! Each copy is made as `<clones>/<mind>/<hand>.<code>.part`, and takes its
//! name, `<hand>.<code>`, once it is whole: a directory named so is a
//! clone's, and [`copies`] finds them.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

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
    /// `<clones>/<mind>/<hand>.<code>` and started through `spawn`.
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
    /// Copies the directory once per target, then starts a hand on each
    /// copy. Starting is quick once every copy is made, so a clone does not
    /// wait on the copies after it for the mind it joins.
    async fn clone_into(&self, into: Vec<Target>) -> Reply {
        // Undone if this future is dropped or fails before the end.
        let mut made = Made::default();
        let mut copies = Vec::new();
        for Target { mind, hand } in into {
            // A directory of its own, whatever another run left here.
            let dir = self
                .clones
                .join(mind.as_str())
                .join(format!("{hand}.{}", suffix()));
            let part = partial(&dir);
            made.dirs.extend([dir.clone(), part.clone()]);
            let (from, skip) = (self.directory.clone(), self.clones.clone());
            let (to, stop) = (dir.clone(), Arc::clone(&made.stop));
            let ended = tokio::task::spawn_blocking(move || {
                copy_tree(&from, &part, &skip, &stop)?;
                whole(&part, &to, &stop)
            })
            .await;
            match ended {
                Ok(Ok(())) => copies.push((mind, hand, dir)),
                Ok(Err(error)) => {
                    return Reply::Error(format!("the directory could not be copied: {error}"));
                }
                Err(error) => return Reply::Error(format!("the copy stopped: {error}")),
            }
        }
        let mut clones = Vec::new();
        for (mind, hand, dir) in copies {
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

/// A clone's copy found under a clones directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Copied {
    /// The mind its clone joins.
    pub mind: Name,
    /// Its clone's name in that mind.
    pub hand: Name,
    /// The copy.
    pub dir: PathBuf,
}

/// Every whole copy under `clones` whose mind descends from `root`, and
/// removes what a copy cut short left of its own. A clones directory that
/// does not exist holds none. What cannot be read or removed is passed
/// over, and logged.
pub fn copies(clones: &Path, root: &str) -> std::io::Result<Vec<Copied>> {
    let mut found = Vec::new();
    let minds = match std::fs::read_dir(clones) {
        Ok(minds) => minds,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(found),
        Err(error) => return Err(error),
    };
    for entry in minds.flatten() {
        let named = entry.file_name();
        let Some(mind) = named.to_str().and_then(|name| Name::new(name).ok()) else {
            continue;
        };
        if mind.root() != root || !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let copies = match std::fs::read_dir(entry.path()) {
            Ok(copies) => copies,
            Err(error) => {
                tracing::warn!(dir = %entry.path().display(), %error, "copies cannot be read");
                continue;
            }
        };
        for copy in copies.flatten() {
            let (dir, name) = (copy.path(), copy.file_name());
            let Some(name) = name.to_str() else {
                continue;
            };
            if !copy.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            match name.strip_suffix(PART).map(copy_of) {
                Some(Some(_)) => {
                    if let Err(error) = remove_tree(&dir) {
                        let dir = dir.display();
                        tracing::warn!(%dir, %error, "a copy cut short cannot be removed");
                    }
                }
                Some(None) => {}
                None => {
                    if let Some(hand) = copy_of(name) {
                        found.push(Copied {
                            mind: mind.clone(),
                            hand,
                            dir,
                        });
                    }
                }
            }
        }
    }
    found.sort_by(|a, b| a.dir.cmp(&b.dir));
    Ok(found)
}

/// The hand a copy named `name`, `<hand>.<code>`, is for.
fn copy_of(name: &str) -> Option<Name> {
    let (hand, code) = name.rsplit_once('.')?;
    if code.len() != 8 || !code.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Name::new(hand).ok()
}

/// Removes the tree at `path`, a directory in it that cannot be written to
/// included, and nothing a link in it points to. A tree that is not there
/// is removed.
pub fn remove_tree(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            let _ = writable(path);
            std::fs::remove_dir_all(path)
        }
        Err(error) => Err(error),
    }
}

/// Lets its owner change every directory in the tree at `path`.
fn writable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() {
        return Ok(());
    }
    let mode = metadata.permissions().mode();
    if mode & 0o700 != 0o700 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode | 0o700))?;
    }
    for entry in std::fs::read_dir(path)?.flatten() {
        let _ = writable(&entry.path());
    }
    Ok(())
}

/// Gives the copy made at `part` its whole name, `to`, unless its clone was
/// undone. An undo that came first finds the copy gone, and one that comes
/// after finds it under its whole name.
fn whole(part: &Path, to: &Path, stop: &Stop) -> std::io::Result<()> {
    let stopped = stop.0.lock().unwrap_or_else(PoisonError::into_inner);
    if *stopped {
        let _ = remove_tree(part);
        return Err(std::io::Error::other("the clone was stopped"));
    }
    std::fs::rename(part, to)
}

/// Whether a clone was undone. It is set under its lock, which a copy holds
/// while it takes its whole name.
#[derive(Default)]
struct Stop(Mutex<bool>);

impl Stop {
    fn is_set(&self) -> bool {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set(&self) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = true;
    }
}

/// What a copy is named while it is made.
const PART: &str = ".part";

/// Where the copy that will be `dir` is made.
fn partial(dir: &Path) -> PathBuf {
    let mut part = dir.as_os_str().to_os_string();
    part.push(PART);
    PathBuf::from(part)
}

/// Eight random hex characters.
fn suffix() -> String {
    let mut bytes = [0u8; 4];
    let _ = getrandom::fill(&mut bytes);
    bytes
        .iter()
        .fold(String::with_capacity(8), |mut hex, byte| {
            use std::fmt::Write;
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// What a clone has made so far. Dropped before it is kept, it undoes all
/// of it, so a clone makes every copy or none, failed or cancelled: it
/// stops a copy still being made, stops the hands it started, and removes
/// the copies.
#[derive(Default)]
struct Made {
    dirs: Vec<PathBuf>,
    started: Vec<Box<dyn Started>>,
    /// Tells a copy still being made to stop, and remove what it made.
    stop: Arc<Stop>,
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
        self.stop.set();
        for started in self.started.drain(..) {
            started.stop();
        }
        let dirs = std::mem::take(&mut self.dirs);
        let remove = move || {
            for dir in dirs {
                let _ = remove_tree(&dir);
            }
        };
        // Off the runtime's threads, where there is one: a large copy takes
        // a while to remove.
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => drop(runtime.spawn_blocking(remove)),
            Err(_) => remove(),
        }
    }
}

/// Copies the tree at `from` to `to`, which must not exist, leaving out
/// `skip` if it is inside it, until `stop` is set. What was copied is
/// removed again if the copy fails or is stopped.
fn copy_tree(from: &Path, to: &Path, skip: &Path, stop: &Stop) -> std::io::Result<()> {
    std::fs::create_dir_all(skip)?;
    let skip = std::fs::canonicalize(skip)?;
    if skip == std::fs::canonicalize(from)? {
        return Err(std::io::Error::other(
            "the clones directory is the hand's own directory",
        ));
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let copied = copy_dir(from, to, &skip, stop);
    if copied.is_err() {
        let _ = remove_tree(to);
    }
    copied
}

/// Copies a directory: files, links as links, and directories with their
/// permissions. A pipe, a socket or a device is no file to copy, and is
/// left out.
fn copy_dir(from: &Path, to: &Path, skip: &Path, stop: &Stop) -> std::io::Result<()> {
    std::fs::create_dir(to)?;
    for entry in std::fs::read_dir(from)? {
        if stop.is_set() {
            return Err(std::io::Error::other("the clone was stopped"));
        }
        let entry = entry?;
        let (source, target) = (entry.path(), to.join(entry.file_name()));
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(&source)?, &target)?;
        } else if kind.is_dir() {
            if std::fs::canonicalize(&source)? != skip {
                copy_dir(&source, &target, skip, stop)?;
            }
        } else if kind.is_file() {
            std::fs::copy(&source, &target)?;
        }
    }
    std::fs::set_permissions(to, std::fs::metadata(from)?.permissions())
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
        let copy = PathBuf::from(output["clones"][1]["dir"].as_str().unwrap());
        assert!(copy.starts_with(root.path().join("clones/m.b")));
        assert!(
            copy.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("a1.")
        );
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
        eventually_empty(&root.path().join("clones/m.a")).await;
        eventually_empty(&root.path().join("clones/m.b")).await;
    }

    /// Waits until `dir` holds nothing: removal happens off the runtime.
    async fn eventually_empty(dir: &Path) {
        for _ in 0..200 {
            if std::fs::read_dir(dir).map_or(true, |mut entries| entries.next().is_none()) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("{} still holds something", dir.display());
    }

    #[tokio::test]
    async fn a_clone_runs_again_beside_what_an_earlier_one_left() {
        let (root, cloning, _) = hand(Spawner::default());
        let first = cloning.call("clone", into(&[("m.a", "a1")])).await;
        let again = cloning.call("clone", into(&[("m.a", "a1")])).await;
        let dir = |reply: &Reply| match reply {
            Reply::Output(output) => output["clones"][0]["dir"].as_str().unwrap().to_owned(),
            Reply::Error(error) => panic!("{error}"),
        };
        assert_ne!(dir(&first), dir(&again));
        assert_eq!(
            std::fs::read_dir(root.path().join("clones/m.a"))
                .unwrap()
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn what_is_no_file_is_left_out_and_permissions_are_kept() {
        use std::os::unix::fs::PermissionsExt;

        let (root, cloning, _) = hand(Spawner::default());
        let work = root.path().join("work");
        let piped = std::process::Command::new("mkfifo")
            .arg(work.join("pipe"))
            .status()
            .unwrap();
        assert!(piped.success());
        std::fs::set_permissions(work.join("src"), std::fs::Permissions::from_mode(0o700)).unwrap();
        let reply = cloning.call("clone", into(&[("m.a", "a1")])).await;
        let Reply::Output(output) = reply else {
            panic!("{reply:?}");
        };
        let copy = PathBuf::from(output["clones"][0]["dir"].as_str().unwrap());
        assert!(!copy.join("pipe").exists());
        let bits = std::fs::metadata(copy.join("src"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(bits & 0o777, 0o700);
    }

    #[tokio::test]
    async fn whole_copies_of_one_roots_minds_are_found_and_cut_ones_removed() {
        let (root, cloning, _) = hand(Spawner::default());
        let reply = cloning
            .call(
                "clone",
                into(&[("m", "a1-2"), ("m.d3342iuj", "a1"), ("n.zzzzzzzz", "a1")]),
            )
            .await;
        assert!(matches!(reply, Reply::Output(_)), "{reply:?}");
        let clones = root.path().join("clones");
        let cut = clones.join("m/a1-3.0badc0de.part");
        std::fs::create_dir_all(&cut).unwrap();
        std::fs::create_dir_all(clones.join("m/not-a-copy")).unwrap();

        let found = copies(&clones, "m").unwrap();
        let named: Vec<(&str, &str)> = found
            .iter()
            .map(|copy| (copy.mind.as_str(), copy.hand.as_str()))
            .collect();
        assert_eq!(named, [("m", "a1-2"), ("m.d3342iuj", "a1")]);
        assert!(found[0].dir.join("notes.txt").exists());
        assert!(!cut.exists());
        assert!(copies(&root.path().join("none"), "m").unwrap().is_empty());
    }

    #[test]
    fn a_copy_undone_before_it_is_whole_never_takes_its_name() {
        let root = tempfile::tempdir().unwrap();
        let (part, to) = (
            root.path().join("a1.0badc0de.part"),
            root.path().join("a1.0badc0de"),
        );
        std::fs::create_dir(&part).unwrap();
        let stop = Stop::default();
        stop.set();
        assert!(whole(&part, &to, &stop).is_err());
        assert!(!part.exists() && !to.exists());

        std::fs::create_dir(&part).unwrap();
        whole(&part, &to, &Stop::default()).unwrap();
        assert!(!part.exists() && to.exists());
    }

    #[test]
    fn a_tree_with_a_directory_no_one_may_write_to_is_removed() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let clones = root.path().join("clones");
        let cut = clones.join("m/a1.0badc0de.part");
        std::fs::create_dir_all(cut.join("cache")).unwrap();
        std::fs::write(cut.join("cache/mod.go"), "package m").unwrap();
        std::fs::set_permissions(cut.join("cache"), std::fs::Permissions::from_mode(0o555))
            .unwrap();
        // A directory named like no copy is not the clone's to remove.
        let theirs = clones.join("m/notes.part");
        std::fs::create_dir_all(&theirs).unwrap();

        assert!(copies(&clones, "m").unwrap().is_empty());
        assert!(!cut.exists());
        assert!(theirs.exists());
        remove_tree(&root.path().join("not there")).unwrap();
    }

    #[tokio::test]
    async fn clones_kept_inside_the_directory_are_not_copied_into_clones() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("notes.txt"), "hi").unwrap();
        let spawner = Arc::new(Spawner::default());
        let cloning = Cloning::new(root.path(), root.path().join(".clones"), spawner);
        let reply = cloning.call("clone", into(&[("m.a", "a1")])).await;
        let Reply::Output(output) = reply else {
            panic!("{reply:?}");
        };
        let copy = PathBuf::from(output["clones"][0]["dir"].as_str().unwrap());
        assert!(copy.join("notes.txt").exists());
        assert!(!copy.join(".clones").exists());
    }

    #[tokio::test]
    async fn clones_kept_in_the_directory_itself_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let spawner = Arc::new(Spawner::default());
        let cloning = Cloning::new(root.path(), root.path(), spawner);
        let reply = cloning.call("clone", into(&[("m.a", "a1")])).await;
        assert!(
            matches!(&reply, Reply::Error(error) if error.contains("own directory")),
            "{reply:?}"
        );
    }
}

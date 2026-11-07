//! Where change events come from.
//!
//! Every backend implements [`ChangeSource`] and reports [`SourceEvent`]s through one sink,
//! so the engine never depends on how an operating system watches files:
//!
//! * [`RecursiveSource`]: one native recursive watch per tree (`FSEvents` on macOS,
//!   `ReadDirectoryChangesW` on Windows), filtering ignored paths as events arrive.
//! * [`PrunedSource`]: one watch per directory that git does not ignore (inotify on Linux,
//!   where recursive watching would watch ignored trees such as `target/` and exhaust the
//!   per-user watch limit), following directories as they appear.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;

use notify::{RecursiveMode, Watcher};

use crate::filter::IgnoreFilter;

/// What a watched location is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchKind {
    /// A worktree; ignored directories inside it need no watching.
    Worktree {
        /// Common git directory of its repository (for `info/exclude`).
        common_dir: PathBuf,
    },
    /// A git directory; object storage and logs inside it need no watching.
    GitDir,
}

/// A location to watch, everything beneath it included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchTarget {
    /// Path, symbolic links resolved.
    pub path: PathBuf,
    /// What it is.
    pub kind: WatchKind,
}

/// What a change source reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceEvent {
    /// These paths (files or directories) changed.
    Changed(Vec<PathBuf>),
    /// Events were lost under this path (everywhere when `None`); check it all again.
    Rescan(Option<PathBuf>),
    /// Watching is incomplete (for example a system watch limit was reached).
    Degraded(String),
}

/// Receives a change source's events, from any thread.
pub type EventSink = Arc<dyn Fn(SourceEvent) + Send + Sync>;

/// Failure to start watching.
#[derive(Debug, thiserror::Error)]
#[error("watching files failed: {0}")]
pub struct SourceError(String);

impl From<notify::Error> for SourceError {
    fn from(error: notify::Error) -> Self {
        Self(error.to_string())
    }
}

/// Reports changes under watched locations.
pub trait ChangeSource: Send {
    /// Short name for diagnostics.
    fn name(&self) -> &'static str;

    /// Start watching `target`.
    ///
    /// # Errors
    /// * [`SourceError`] when the location cannot be watched at all.
    fn watch(&mut self, target: &WatchTarget) -> Result<(), SourceError>;

    /// Stop watching a location given to [`Self::watch`].
    fn unwatch(&mut self, path: &Path);
}

/// Which [`ChangeSource`] to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// The best backend for this operating system.
    #[default]
    Auto,
    /// [`RecursiveSource`].
    Recursive,
    /// [`PrunedSource`].
    Pruned,
}

/// A change source of `kind`, reporting to `sink`.
///
/// # Errors
/// * [`SourceError`] when the operating system's watcher cannot start.
pub fn open(kind: SourceKind, sink: EventSink) -> Result<Box<dyn ChangeSource>, SourceError> {
    let recursive = match kind {
        SourceKind::Auto => cfg!(any(target_os = "macos", target_os = "windows")),
        SourceKind::Recursive => true,
        SourceKind::Pruned => false,
    };
    Ok(if recursive {
        Box::new(RecursiveSource::new(sink)?)
    } else {
        Box::new(PrunedSource::new(sink)?)
    })
}

/// Turn a native event into source events.
fn forward(result: notify::Result<notify::Event>, sink: &EventSink) {
    match result {
        Ok(event) => {
            if matches!(event.kind, notify::EventKind::Access(_)) {
                return;
            }
            if event.need_rescan() {
                sink(SourceEvent::Rescan(event.paths.into_iter().next()));
            } else if !event.paths.is_empty() {
                sink(SourceEvent::Changed(event.paths));
            }
        }
        Err(error) => {
            sink(SourceEvent::Degraded(error.to_string()));
            sink(SourceEvent::Rescan(error.paths.into_iter().next()));
        }
    }
}

/// One native recursive watch per tree. Trees inside another watched tree share its watch.
pub struct RecursiveSource {
    watcher: notify::RecommendedWatcher,
    /// Locations asked for, and whether each holds its own native watch.
    targets: BTreeMap<PathBuf, bool>,
}

impl RecursiveSource {
    /// A source reporting to `sink`.
    ///
    /// # Errors
    /// * [`SourceError`] when the operating system's watcher cannot start.
    pub fn new(sink: EventSink) -> Result<Self, SourceError> {
        let watcher = notify::recommended_watcher(move |result| forward(result, &sink))?;
        Ok(Self {
            watcher,
            targets: BTreeMap::new(),
        })
    }

    fn covered(&self, path: &Path) -> bool {
        self.targets
            .iter()
            .any(|(target, native)| *native && target != path && path.starts_with(target))
    }
}

impl ChangeSource for RecursiveSource {
    fn name(&self) -> &'static str {
        "recursive"
    }

    fn watch(&mut self, target: &WatchTarget) -> Result<(), SourceError> {
        if self.targets.contains_key(&target.path) {
            return Ok(());
        }
        let native = !self.covered(&target.path);
        if native {
            self.watcher.watch(&target.path, RecursiveMode::Recursive)?;
        }
        self.targets.insert(target.path.clone(), native);
        Ok(())
    }

    fn unwatch(&mut self, path: &Path) {
        if self.targets.remove(path) == Some(true) {
            let _ = self.watcher.unwatch(path);
            // Trees it covered need their own watch now.
            let uncovered: Vec<PathBuf> = self
                .targets
                .iter()
                .filter(|(target, native)| !**native && target.starts_with(path))
                .map(|(target, _)| target.clone())
                .collect();
            for target in uncovered {
                let native = !self.covered(&target);
                if native
                    && self
                        .watcher
                        .watch(&target, RecursiveMode::Recursive)
                        .is_ok()
                {
                    self.targets.insert(target, true);
                }
            }
        }
    }
}

/// Git directories whose contents never matter to live state.
const GIT_DIR_SKIPPED: &[&str] = &["objects", "logs", "hooks", "lfs", "modules"];

enum Command {
    Watch(WatchTarget, mpsc::Sender<Result<(), SourceError>>),
    Unwatch(PathBuf),
    Native(notify::Result<notify::Event>),
    Stop,
}

/// One watch per directory git does not ignore, maintained by a worker thread as
/// directories appear, disappear, or become ignored or not.
pub struct PrunedSource {
    commands: mpsc::Sender<Command>,
}

impl PrunedSource {
    /// A source reporting to `sink`.
    ///
    /// # Errors
    /// * [`SourceError`] when the operating system's watcher cannot start.
    pub fn new(sink: EventSink) -> Result<Self, SourceError> {
        let (commands, receiver) = mpsc::channel();
        let native = commands.clone();
        let watcher = notify::recommended_watcher(move |result| {
            let _ = native.send(Command::Native(result));
        })?;
        std::thread::Builder::new()
            .name("bcode-git-watch".into())
            .spawn(move || {
                Pruner {
                    watcher,
                    sink,
                    targets: Vec::new(),
                    watched: BTreeSet::new(),
                    degraded: false,
                }
                .run(&receiver);
            })
            .map_err(|error| SourceError(error.to_string()))?;
        Ok(Self { commands })
    }
}

impl Drop for PrunedSource {
    fn drop(&mut self) {
        // The native watcher's callback keeps the channel open, so stop explicitly.
        let _ = self.commands.send(Command::Stop);
    }
}

impl ChangeSource for PrunedSource {
    fn name(&self) -> &'static str {
        "pruned"
    }

    fn watch(&mut self, target: &WatchTarget) -> Result<(), SourceError> {
        let (reply, answer) = mpsc::channel();
        self.commands
            .send(Command::Watch(target.clone(), reply))
            .map_err(|_| SourceError("the watcher stopped".into()))?;
        answer
            .recv()
            .map_err(|_| SourceError("the watcher stopped".into()))?
    }

    fn unwatch(&mut self, path: &Path) {
        let _ = self.commands.send(Command::Unwatch(path.to_owned()));
    }
}

struct Pruner {
    watcher: notify::RecommendedWatcher,
    sink: EventSink,
    targets: Vec<(WatchTarget, Option<IgnoreFilter>)>,
    watched: BTreeSet<PathBuf>,
    degraded: bool,
}

impl Pruner {
    fn run(mut self, commands: &mpsc::Receiver<Command>) {
        while let Ok(command) = commands.recv() {
            match command {
                Command::Stop => return,
                Command::Watch(target, reply) => {
                    let result = self.watch(target);
                    let _ = reply.send(result);
                }
                Command::Unwatch(path) => self.unwatch(&path),
                Command::Native(result) => self.native(result),
            }
        }
    }

    fn watch(&mut self, target: WatchTarget) -> Result<(), SourceError> {
        if self
            .targets
            .iter()
            .any(|(existing, _)| existing.path == target.path)
        {
            return Ok(());
        }
        self.watcher
            .watch(&target.path, RecursiveMode::NonRecursive)?;
        let path = target.path.clone();
        self.watched.insert(path.clone());
        let filter = match &target.kind {
            WatchKind::Worktree { common_dir } => Some(IgnoreFilter::new(&path, common_dir)),
            WatchKind::GitDir => None,
        };
        self.targets.push((target, filter));
        self.add_tree(self.targets.len() - 1, &path);
        Ok(())
    }

    fn unwatch(&mut self, path: &Path) {
        self.targets.retain(|(target, _)| target.path != path);
        let gone: Vec<PathBuf> = self
            .watched
            .iter()
            .filter(|watched| watched.starts_with(path) && !self.wanted(watched))
            .cloned()
            .collect();
        for directory in gone {
            let _ = self.watcher.unwatch(&directory);
            self.watched.remove(&directory);
        }
    }

    /// Whether some remaining target still wants `directory` watched.
    fn wanted(&self, directory: &Path) -> bool {
        self.targets
            .iter()
            .any(|(target, _)| directory.starts_with(&target.path))
    }

    /// The target `path` belongs to: the deepest containing one.
    fn target_of(&self, path: &Path) -> Option<usize> {
        self.targets
            .iter()
            .enumerate()
            .filter(|(_, (target, _))| path.starts_with(&target.path))
            .max_by_key(|(_, (target, _))| target.path.components().count())
            .map(|(index, _)| index)
    }

    /// Whether `directory` inside target `index` needs a watch.
    fn wants(&mut self, index: usize, directory: &Path) -> bool {
        let (target, filter) = &mut self.targets[index];
        let Ok(relative) = directory.strip_prefix(&target.path) else {
            return false;
        };
        let Some(first) = relative.components().next() else {
            return true;
        };
        filter.as_mut().map_or_else(
            || {
                !GIT_DIR_SKIPPED
                    .iter()
                    .any(|skipped| first.as_os_str() == *skipped)
            },
            // The worktree's own git directory is watched as a git directory.
            |filter| first.as_os_str() != ".git" && !filter.is_ignored(relative, true),
        )
    }

    /// Watch `directory` and every directory beneath it that target `index` wants.
    fn add_tree(&mut self, index: usize, directory: &Path) {
        let mut stack = vec![directory.to_owned()];
        while let Some(current) = stack.pop() {
            if !self.watched.contains(&current) {
                if let Err(error) = self.watcher.watch(&current, RecursiveMode::NonRecursive) {
                    if !self.degraded {
                        self.degraded = true;
                        (self.sink)(SourceEvent::Degraded(format!(
                            "not every directory can be watched: {error}"
                        )));
                    }
                    continue;
                }
                self.watched.insert(current.clone());
            }
            let Ok(entries) = std::fs::read_dir(&current) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
                // Directories that belong to a deeper target are left to it.
                if is_dir && self.target_of(&path) == Some(index) && self.wants(index, &path) {
                    stack.push(path);
                }
            }
        }
    }

    fn native(&mut self, result: notify::Result<notify::Event>) {
        let paths = result
            .as_ref()
            .map(|event| event.paths.clone())
            .unwrap_or_default();
        forward(result, &self.sink);
        for path in paths {
            if path.file_name().is_some_and(|name| name == ".gitignore") {
                self.ignore_changed(&path);
            } else if path.is_dir() {
                self.directory_appeared(&path);
            } else if self.watched.contains(&path) {
                let gone: Vec<PathBuf> = self
                    .watched
                    .iter()
                    .filter(|watched| watched.starts_with(&path))
                    .cloned()
                    .collect();
                for directory in gone {
                    let _ = self.watcher.unwatch(&directory);
                    self.watched.remove(&directory);
                }
            }
        }
    }

    /// A directory was created or moved in: watch it, then report it again so files made
    /// before its watch started are seen.
    fn directory_appeared(&mut self, directory: &Path) {
        if self.watched.contains(directory) {
            return;
        }
        let Some(index) = self.target_of(directory) else {
            return;
        };
        if !self.wants(index, directory) {
            return;
        }
        self.add_tree(index, directory);
        (self.sink)(SourceEvent::Changed(vec![directory.to_owned()]));
    }

    /// Rules changed: directories they no longer ignore need watching.
    fn ignore_changed(&mut self, file: &Path) {
        let Some(index) = self.target_of(file) else {
            return;
        };
        if let Some(filter) = &mut self.targets[index].1 {
            filter.reset();
        }
        if let Some(directory) = file.parent() {
            let directory = directory.to_owned();
            self.add_tree(index, &directory);
        }
    }
}

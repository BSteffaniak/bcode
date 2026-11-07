//! An engine and a change source on their own thread.

use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use crate::engine::{Engine, LiveUpdate, Pending, Request, WatchChange};
use crate::source::{ChangeSource, SourceEvent, SourceKind, open};

/// Quiet time after the last event before work starts, so a burst becomes one batch.
const QUIET: Duration = Duration::from_millis(15);
/// Longest a change waits while events keep arriving.
const LATEST: Duration = Duration::from_millis(100);

enum Input {
    Source(SourceEvent),
    Request(Request),
    Stop,
}

/// Runs an [`Engine`] on its own thread, reporting each non-empty [`LiveUpdate`] to a sink.
///
/// The engine is fed by a change source and by requests. Events that arrive while work runs
/// are batched for the next round, so work never overlaps and bursts coalesce.
pub struct LocalSession {
    inputs: mpsc::Sender<Input>,
    watching: bool,
}

impl LocalSession {
    /// Start `engine`, watching with `source` (no watching when `None`), and scan
    /// everything once. When the watcher cannot start, the session runs without it, says so
    /// through [`Self::watching`], and reports why in its first update.
    #[must_use]
    pub fn start(
        engine: Engine,
        source: Option<SourceKind>,
        sink: impl Fn(LiveUpdate) + Send + 'static,
    ) -> Self {
        let (inputs, receiver) = mpsc::channel();
        let events = inputs.clone();
        let mut errors = Vec::new();
        let source = source.and_then(|kind| {
            open(
                kind,
                Arc::new(move |event| {
                    let _ = events.send(Input::Source(event));
                }),
            )
            .map_err(|error| errors.push(error.to_string()))
            .ok()
        });
        let watching = source.is_some();
        let _ = inputs.send(Input::Request(Request::Full));
        let spawned = std::thread::Builder::new()
            .name("bcode-git-live".into())
            .spawn(move || run(engine, source, &receiver, &sink, errors));
        drop(spawned);
        Self { inputs, watching }
    }

    /// Whether changes are watched (otherwise only requests bring updates).
    #[must_use]
    pub const fn watching(&self) -> bool {
        self.watching
    }

    /// Ask for work beyond reacting to changes.
    pub fn request(&self, request: Request) {
        let _ = self.inputs.send(Input::Request(request));
    }
}

impl Drop for LocalSession {
    fn drop(&mut self) {
        let _ = self.inputs.send(Input::Stop);
    }
}

fn run(
    mut engine: Engine,
    mut source: Option<Box<dyn ChangeSource>>,
    inputs: &mpsc::Receiver<Input>,
    sink: &impl Fn(LiveUpdate),
    mut errors: Vec<String>,
) {
    loop {
        let Ok(first) = inputs.recv() else {
            return;
        };
        let mut pending = Pending::default();
        if !take(&mut engine, &mut pending, &mut errors, first) {
            return;
        }
        let started = Instant::now();
        loop {
            let wait = QUIET.min(LATEST.saturating_sub(started.elapsed()));
            match inputs.recv_timeout(wait) {
                Ok(input) => {
                    if !take(&mut engine, &mut pending, &mut errors, input) {
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
            if started.elapsed() >= LATEST {
                break;
            }
        }
        // Watch newly kept worktrees before scanning them, so no change slips between.
        engine.prepare(&mut pending);
        register(&mut engine, &mut source, &mut errors);
        let mut update = if pending.is_empty() {
            LiveUpdate::default()
        } else {
            engine.apply(pending)
        };
        register(&mut engine, &mut source, &mut errors);
        update.errors.append(&mut errors);
        if !update.is_empty() {
            sink(update);
        }
    }
}

/// Start or stop the watches the engine asks for.
fn register(
    engine: &mut Engine,
    source: &mut Option<Box<dyn ChangeSource>>,
    errors: &mut Vec<String>,
) {
    let changes = engine.take_watch_changes();
    let Some(source) = source else {
        return;
    };
    for change in changes {
        match change {
            WatchChange::Watch(target) => {
                if let Err(error) = source.watch(&target) {
                    errors.push(error.to_string());
                }
            }
            WatchChange::Unwatch(path) => source.unwatch(&path),
        }
    }
}

/// Add one input to `pending`; `false` to stop.
fn take(
    engine: &mut Engine,
    pending: &mut Pending,
    errors: &mut Vec<String>,
    input: Input,
) -> bool {
    match input {
        Input::Stop => return false,
        Input::Request(request) => pending.request(request),
        Input::Source(SourceEvent::Changed(paths)) => {
            for path in paths {
                engine.route(&path, pending);
            }
        }
        Input::Source(SourceEvent::Rescan(path)) => engine.rescan(path.as_deref(), pending),
        Input::Source(SourceEvent::Degraded(message)) => errors.push(message),
    }
    true
}

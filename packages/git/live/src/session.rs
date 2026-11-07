//! Live state run for clients: a [`Hub`] thread serving any number of subscribers of one
//! repository, and the single-client sessions built on it.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use bcode_git::Repository;

use crate::counts::CountsMode;
use crate::engine::{Engine, EngineConfig, Interest, LiveUpdate, Pending, Request, WatchChange};
use crate::scan::ScannerKind;
use crate::source::{ChangeSource, SourceEvent, SourceKind, open};
use crate::subscriber::Subscriber;

/// Quiet time after the last event before work starts, so a burst becomes one batch.
const QUIET: Duration = Duration::from_millis(15);
/// Longest a change waits while events keep arriving.
const LATEST: Duration = Duration::from_millis(100);

/// Receives a subscriber's updates, from the hub's thread.
pub type UpdateSink = Arc<dyn Fn(LiveUpdate) + Send + Sync>;

/// Live state for one client, however it is served.
pub trait LiveSession: Send {
    /// Ask for work beyond reacting to changes.
    fn request(&self, request: Request);

    /// Whether changes are watched (otherwise only requests bring updates).
    fn watching(&self) -> bool;
}

enum Input {
    Source(SourceEvent),
    Request(Request),
    Subscribe(u64, Repository, EngineConfig, UpdateSink),
    Counts(u64, CountsMode),
    Unsubscribe(u64),
    Stop,
}

/// One repository's live state on its own thread, shared by any number of subscribers.
///
/// Change events and requests are batched (after [`QUIET`] without events, at most
/// [`LATEST`] after the first), so work never overlaps and bursts coalesce; each subscriber
/// then gets what changed for it. Dropping the hub stops it.
pub struct Hub {
    inputs: mpsc::Sender<Input>,
    watching: bool,
    next: AtomicU64,
}

impl Hub {
    /// Start live state for the repository `repo` belongs to, watching with `source` (no
    /// watching when `None` or when the watcher cannot start; see [`Self::watching`]).
    /// Failures to watch reach subscribers as [`LiveUpdate::errors`].
    #[must_use]
    pub fn start(repo: &Repository, scanner: ScannerKind, source: Option<SourceKind>) -> Self {
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
        let engine = Engine::new(repo, scanner);
        let spawned = std::thread::Builder::new()
            .name("bcode-git-live".into())
            .spawn(move || Loop::new(engine, source, errors).run(&receiver));
        drop(spawned);
        Self {
            inputs,
            watching,
            next: AtomicU64::new(1),
        }
    }

    /// Whether changes are watched.
    #[must_use]
    pub const fn watching(&self) -> bool {
        self.watching
    }

    /// Serve a client whose own worktree is `repo`, wanting `config`, reporting to `sink`.
    /// It first gets everything known, then changes. Returns its id.
    pub fn subscribe(&self, repo: &Repository, config: EngineConfig, sink: UpdateSink) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let _ = self
            .inputs
            .send(Input::Subscribe(id, repo.clone(), config, sink));
        id
    }

    /// Stop serving client `id`.
    pub fn unsubscribe(&self, id: u64) {
        let _ = self.inputs.send(Input::Unsubscribe(id));
    }

    /// Carry out `request` for client `id`; [`Request::Counts`] concerns that client alone.
    pub fn request(&self, id: u64, request: Request) {
        let _ = self.inputs.send(match request {
            Request::Counts(counts) => Input::Counts(id, counts),
            other => Input::Request(other),
        });
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        let _ = self.inputs.send(Input::Stop);
    }
}

struct Client {
    repo: Repository,
    subscriber: Subscriber,
    sink: UpdateSink,
    /// Has not been told anything yet.
    new: bool,
}

struct Loop {
    engine: Engine,
    source: Option<Box<dyn ChangeSource>>,
    clients: BTreeMap<u64, Client>,
    errors: Vec<String>,
}

#[derive(Default)]
struct Round {
    pending: Pending,
    interests: bool,
}

impl Loop {
    fn new(engine: Engine, source: Option<Box<dyn ChangeSource>>, errors: Vec<String>) -> Self {
        Self {
            engine,
            source,
            clients: BTreeMap::new(),
            errors,
        }
    }

    fn run(mut self, inputs: &mpsc::Receiver<Input>) {
        loop {
            let Ok(first) = inputs.recv() else {
                return;
            };
            let mut round = Round::default();
            if !self.take(&mut round, first) {
                return;
            }
            let started = Instant::now();
            loop {
                let wait = QUIET.min(LATEST.saturating_sub(started.elapsed()));
                match inputs.recv_timeout(wait) {
                    Ok(input) => {
                        if !self.take(&mut round, input) {
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
            self.work(round);
        }
    }

    /// Add one input to `round`; `false` to stop.
    fn take(&mut self, round: &mut Round, input: Input) -> bool {
        match input {
            Input::Stop => return false,
            Input::Request(request) => round.pending.request(request),
            Input::Source(SourceEvent::Changed(paths)) => {
                for path in paths {
                    self.engine.route(&path, &mut round.pending);
                }
            }
            Input::Source(SourceEvent::Rescan(path)) => {
                self.engine.rescan(path.as_deref(), &mut round.pending);
            }
            Input::Source(SourceEvent::Degraded(message)) => self.errors.push(message),
            Input::Subscribe(id, repo, config, sink) => {
                let subscriber = Subscriber::new(Engine::root_of(&repo), config);
                self.clients.insert(
                    id,
                    Client {
                        repo,
                        subscriber,
                        sink,
                        new: true,
                    },
                );
                round.interests = true;
            }
            Input::Counts(id, counts) => {
                if let Some(client) = self.clients.get_mut(&id) {
                    let mut config = client.subscriber.config();
                    config.counts = counts;
                    client.subscriber.set_config(config);
                    round.interests = true;
                }
            }
            Input::Unsubscribe(id) => {
                round.interests |= self.clients.remove(&id).is_some();
            }
        }
        true
    }

    fn work(&mut self, mut round: Round) {
        if round.interests {
            let interests = self
                .clients
                .values()
                .map(|client| Interest {
                    repo: client.repo.clone(),
                    config: client.subscriber.config(),
                })
                .collect();
            self.engine.set_interests(interests);
            let fsmonitor = self
                .clients
                .values()
                .any(|client| client.subscriber.config().scanner == ScannerKind::Fsmonitor);
            self.engine.set_scanner(if fsmonitor {
                ScannerKind::Fsmonitor
            } else {
                ScannerKind::Plain
            });
        }
        // Watch newly kept worktrees before scanning them, so no change slips between.
        self.engine.prepare(&mut round.pending);
        self.register();
        let mut applied = self.engine.apply(round.pending);
        self.register();
        applied
            .errors
            .extend(self.errors.drain(..).map(|message| (None, message)));
        for client in self.clients.values_mut() {
            let update = client.subscriber.report(&self.engine, &applied);
            if client.new || !update.is_empty() {
                client.new = false;
                (client.sink)(update);
            }
        }
    }

    /// Start or stop the watches the engine asks for.
    fn register(&mut self) {
        let changes = self.engine.take_watch_changes();
        let Some(source) = self.source.as_mut() else {
            return;
        };
        for change in changes {
            match change {
                WatchChange::Watch(target) => {
                    if let Err(error) = source.watch(&target) {
                        self.errors.push(error.to_string());
                    }
                }
                WatchChange::Unwatch(path) => source.unwatch(&path),
            }
        }
    }
}

/// Live state for a single client, served in this process by its own [`Hub`].
pub struct LocalSession {
    hub: Hub,
    id: u64,
}

impl LocalSession {
    /// Serve the worktree of `repo` as `config` asks, watching with `source` (no watching
    /// when `None`), reporting to `sink`. Everything is scanned once first. When the watcher
    /// cannot start, the session runs without it, says so through
    /// [`LiveSession::watching`], and reports why in its first update.
    #[must_use]
    pub fn start(
        repo: &Repository,
        config: EngineConfig,
        source: Option<SourceKind>,
        sink: UpdateSink,
    ) -> Self {
        let hub = Hub::start(repo, config.scanner, source);
        let id = hub.subscribe(repo, config, sink);
        Self { hub, id }
    }
}

impl LiveSession for LocalSession {
    fn request(&self, request: Request) {
        self.hub.request(self.id, request);
    }

    fn watching(&self) -> bool {
        self.hub.watching()
    }
}

/// Live state for a single client run synchronously on the caller's thread, without
/// watching: each request returns its update. For tests and headless use.
pub struct InlineSession {
    engine: Engine,
    repo: Repository,
    subscriber: Subscriber,
}

impl InlineSession {
    /// Serve the worktree of `repo` as `config` asks.
    #[must_use]
    pub fn new(repo: &Repository, config: EngineConfig) -> Self {
        let mut engine = Engine::new(repo, config.scanner);
        engine.set_interests(vec![Interest {
            repo: repo.clone(),
            config,
        }]);
        Self {
            engine,
            repo: repo.clone(),
            subscriber: Subscriber::new(Engine::root_of(repo), config),
        }
    }

    /// Carry out `request` now and say what changed.
    pub fn request(&mut self, request: Request) -> LiveUpdate {
        let mut pending = Pending::default();
        match request {
            Request::Counts(counts) => {
                let mut config = self.subscriber.config();
                config.counts = counts;
                self.subscriber.set_config(config);
                self.engine.set_interests(vec![Interest {
                    repo: self.repo.clone(),
                    config,
                }]);
            }
            other => pending.request(other),
        }
        let applied = self.engine.apply(pending);
        self.subscriber.report(&self.engine, &applied)
    }
}

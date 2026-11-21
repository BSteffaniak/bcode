//! A native change source checked for liveness, polling while it delivers nothing.
//!
//! An operating system's file events can stop without any error: macOS's `fseventsd` can
//! wedge so that every stream, old or new, starts fine and never reports a change. A
//! [`GuardedSource`] watches a private probe directory with the same native watcher as the
//! repository. After [`GuardTiming::idle`] without native events it writes a probe file; when
//! [`GuardTiming::attempts`] probes in a row go unseen, it watches every location with
//! [`PrunedSource::polling`] instead and reports [`WatchHealth::Polling`]. While polling it
//! starts a fresh native watcher of the probe directory every [`GuardTiming::retry`]; once
//! that one sees its probe (for example after `fseventsd` restarts) it watches everything
//! natively again and reports [`WatchHealth::Native`]. Each switch is followed by
//! [`SourceEvent::Rescan`] of everything, since changes made around it may have been missed.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use super::{
    ChangeSource, EventSink, PrunedSource, SourceError, SourceEvent, WatchCounters, WatchHealth,
    WatchKind, WatchTarget,
};

/// Starts a native change source reporting to the given sink.
pub type NativeFactory =
    Arc<dyn Fn(EventSink) -> Result<Box<dyn ChangeSource>, SourceError> + Send + Sync>;

/// When a [`GuardedSource`] checks native events, and how it finds changes without them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuardTiming {
    /// Native events are checked for with a probe after this long without any.
    pub idle: Duration,
    /// Longest wait for a probe's event.
    pub timeout: Duration,
    /// Probes that must go unseen, one after another, before falling back to polling.
    pub attempts: u32,
    /// While polling, how often native events are tried again.
    pub retry: Duration,
    /// While polling, how often watched directories are compared.
    pub poll: Duration,
}

impl Default for GuardTiming {
    fn default() -> Self {
        Self {
            idle: Duration::from_secs(10),
            timeout: Duration::from_secs(2),
            attempts: 2,
            retry: Duration::from_secs(10),
            poll: Duration::from_secs(1),
        }
    }
}

/// `interval` for people: whole seconds as `2s`, otherwise milliseconds.
pub(super) fn format_interval(interval: Duration) -> String {
    let millis = interval.as_millis();
    if millis.is_multiple_of(1000) {
        format!("{}s", millis / 1000)
    } else {
        format!("{millis}ms")
    }
}

/// A native change source that falls back to polling while the operating system delivers no
/// events, and returns to native events once they flow again. See the module documentation.
pub struct GuardedSource {
    state: Arc<Mutex<State>>,
    stopper: Arc<Stopper>,
    probe_dir: PathBuf,
}

struct State {
    /// Every location asked for, to move between backends.
    targets: Vec<WatchTarget>,
    /// `None` once stopped.
    backend: Option<Backend>,
    /// Work done by backends no longer in use.
    retired: WatchCounters,
}

enum Backend {
    /// A native source, and whether its events are passed on.
    Native(Box<dyn ChangeSource>, Arc<AtomicBool>),
    Polling(PrunedSource),
}

impl Backend {
    fn source(&self) -> &dyn ChangeSource {
        match self {
            Self::Native(source, _) => source.as_ref(),
            Self::Polling(source) => source,
        }
    }

    fn source_mut(&mut self) -> &mut dyn ChangeSource {
        match self {
            Self::Native(source, _) => source.as_mut(),
            Self::Polling(source) => source,
        }
    }
}

impl GuardedSource {
    /// Watch natively with sources `factory` starts, naming the mechanism `native` (such as
    /// `FSEvents`) when it stops delivering events, reporting to `sink`.
    ///
    /// # Errors
    /// * [`SourceError`] when the probe directory cannot be made, the native watcher cannot
    ///   start or watch it, or the supervising thread cannot start.
    pub fn new(
        native: &'static str,
        factory: NativeFactory,
        sink: EventSink,
        timing: GuardTiming,
    ) -> Result<Self, SourceError> {
        let probe_dir = probe_dir()?;
        let pulse = Arc::new(Pulse::default());
        let forwarding = Arc::new(AtomicBool::new(true));
        let started =
            factory(native_sink(&probe_dir, &pulse, &sink, &forwarding)).and_then(|mut source| {
                source.watch(&probe_target(&probe_dir))?;
                Ok(source)
            });
        let source = match started {
            Ok(source) => source,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&probe_dir);
                return Err(error);
            }
        };
        let state = Arc::new(Mutex::new(State {
            targets: Vec::new(),
            backend: Some(Backend::Native(source, forwarding)),
            retired: WatchCounters::default(),
        }));
        let stopper = Arc::new(Stopper::default());
        let supervisor = Supervisor {
            native,
            factory,
            sink,
            timing,
            state: state.clone(),
            stopper: stopper.clone(),
            pulse,
            probe_dir: probe_dir.clone(),
            probes: 0,
            last_events: 0,
        };
        let spawned = std::thread::Builder::new()
            .name("bcode-git-watch-guard".into())
            .spawn(move || supervisor.run());
        if let Err(error) = spawned {
            let _ = std::fs::remove_dir_all(&probe_dir);
            return Err(SourceError(error.to_string()));
        }
        Ok(Self {
            state,
            stopper,
            probe_dir,
        })
    }
}

impl Drop for GuardedSource {
    fn drop(&mut self) {
        self.stopper.stop();
        let backend = lock(&self.state).backend.take();
        if let Some(Backend::Native(source, forwarding)) = backend {
            forwarding.store(false, Ordering::Release);
            retire(source);
        }
        let _ = std::fs::remove_dir_all(&self.probe_dir);
    }
}

impl ChangeSource for GuardedSource {
    fn name(&self) -> &'static str {
        "guarded"
    }

    fn counters(&self) -> WatchCounters {
        let state = lock(&self.state);
        let current = state
            .backend
            .as_ref()
            .map(|backend| backend.source().counters())
            .unwrap_or_default();
        add(state.retired, current)
    }

    fn watch(&mut self, target: &WatchTarget) -> Result<(), SourceError> {
        let mut state = lock(&self.state);
        if state
            .targets
            .iter()
            .any(|existing| existing.path == target.path)
        {
            return Ok(());
        }
        if let Some(backend) = state.backend.as_mut() {
            backend.source_mut().watch(target)?;
        }
        state.targets.push(target.clone());
        drop(state);
        Ok(())
    }

    fn unwatch(&mut self, path: &Path) {
        let mut state = lock(&self.state);
        state.targets.retain(|target| target.path != path);
        if let Some(backend) = state.backend.as_mut() {
            backend.source_mut().unwatch(path);
        }
    }
}

/// Checks native events and switches backends, on its own thread.
struct Supervisor {
    native: &'static str,
    factory: NativeFactory,
    sink: EventSink,
    timing: GuardTiming,
    state: Arc<Mutex<State>>,
    stopper: Arc<Stopper>,
    pulse: Arc<Pulse>,
    probe_dir: PathBuf,
    /// Probes written so far.
    probes: u64,
    /// Native events seen at the last check.
    last_events: u64,
}

impl Supervisor {
    fn run(mut self) {
        // The first check is at once, so a source started while native events are already
        // failing falls back within seconds.
        let mut wait = Duration::ZERO;
        loop {
            if self.stopper.sleep(wait) {
                return;
            }
            let polling = match lock(&self.state).backend {
                None => return,
                Some(Backend::Polling(_)) => true,
                Some(Backend::Native(..)) => false,
            };
            if polling {
                self.try_native();
                wait = self.timing.retry;
            } else {
                if !self.alive() {
                    self.fall_back();
                }
                wait = self.timing.idle;
            }
        }
    }

    /// Whether the native source delivered events since the last check, or sees a probe now.
    fn alive(&mut self) -> bool {
        let events = self.pulse.counts().events;
        if events != self.last_events {
            self.last_events = events;
            return true;
        }
        for _ in 0..self.timing.attempts.max(1) {
            if self.probe() || self.stopper.stopped() {
                return true;
            }
        }
        false
    }

    /// Write the probe file and wait for a native source to report it. A probe that cannot
    /// be written counts as seen: failing to write is no sign native events stopped.
    fn probe(&mut self) -> bool {
        let before = self.pulse.counts().probes;
        self.probes += 1;
        if std::fs::write(self.probe_dir.join("probe"), self.probes.to_string()).is_err() {
            return true;
        }
        self.pulse.await_probe(before, self.timing.timeout)
    }

    fn fall_back(&self) {
        let mut polling = match PrunedSource::polling(self.sink.clone(), self.timing.poll) {
            Ok(polling) => polling,
            Err(error) => {
                (self.sink)(SourceEvent::Degraded(format!(
                    "{} is not delivering file events, and polling could not start: {error}",
                    self.native
                )));
                return;
            }
        };
        let old = {
            let mut state = lock(&self.state);
            if state.backend.is_none() {
                return;
            }
            for target in &state.targets {
                if let Err(error) = polling.watch(target) {
                    (self.sink)(SourceEvent::Degraded(error.to_string()));
                }
            }
            let old = state.backend.replace(Backend::Polling(polling));
            if let Some(old) = &old {
                state.retired = add(state.retired, old.source().counters());
            }
            old
        };
        if let Some(Backend::Native(source, forwarding)) = old {
            forwarding.store(false, Ordering::Release);
            retire(source);
        }
        (self.sink)(SourceEvent::Health(WatchHealth::Polling {
            native: self.native.to_owned(),
            interval_ms: u64::try_from(self.timing.poll.as_millis()).unwrap_or(u64::MAX),
        }));
        (self.sink)(SourceEvent::Rescan(None));
    }

    /// Start a fresh native watcher of the probe directory; when it sees a probe, watch
    /// everything with it instead of polling.
    fn try_native(&mut self) {
        let forwarding = Arc::new(AtomicBool::new(false));
        let Ok(mut trial) = (self.factory)(native_sink(
            &self.probe_dir,
            &self.pulse,
            &self.sink,
            &forwarding,
        )) else {
            return;
        };
        if trial.watch(&probe_target(&self.probe_dir)).is_err() || !self.probe() {
            retire(trial);
            return;
        }
        let old = {
            let mut state = lock(&self.state);
            if state.backend.is_none() {
                drop(state);
                retire(trial);
                return;
            }
            for target in &state.targets {
                if let Err(error) = trial.watch(target) {
                    (self.sink)(SourceEvent::Degraded(error.to_string()));
                }
            }
            forwarding.store(true, Ordering::Release);
            let old = state.backend.replace(Backend::Native(trial, forwarding));
            if let Some(old) = &old {
                state.retired = add(state.retired, old.source().counters());
            }
            old
        };
        drop(old);
        self.last_events = self.pulse.counts().events;
        (self.sink)(SourceEvent::Health(WatchHealth::Native));
        (self.sink)(SourceEvent::Rescan(None));
    }
}

/// A sink for a native source: probe sightings and liveness go to `pulse`; other events go
/// on to `downstream` while `forwarding` holds.
fn native_sink(
    probe_dir: &Path,
    pulse: &Arc<Pulse>,
    downstream: &EventSink,
    forwarding: &Arc<AtomicBool>,
) -> EventSink {
    let probe_dir = probe_dir.to_owned();
    let pulse = pulse.clone();
    let downstream = downstream.clone();
    let forwarding = forwarding.clone();
    Arc::new(move |event| {
        let event = match event {
            SourceEvent::Changed(paths) => {
                let (probes, rest): (Vec<PathBuf>, Vec<PathBuf>) = paths
                    .into_iter()
                    .partition(|path| path.starts_with(&probe_dir));
                if !probes.is_empty() {
                    pulse.note(true);
                }
                if rest.is_empty() {
                    return;
                }
                pulse.note(false);
                SourceEvent::Changed(rest)
            }
            SourceEvent::Rescan(Some(path)) if path.starts_with(&probe_dir) => {
                pulse.note(true);
                return;
            }
            SourceEvent::Rescan(path) => {
                pulse.note(false);
                SourceEvent::Rescan(path)
            }
            other => other,
        };
        if forwarding.load(Ordering::Acquire) {
            downstream(event);
        }
    })
}

/// Stop `source` on a thread of its own: stopping a native stream the operating system no
/// longer serves may block.
fn retire(source: Box<dyn ChangeSource>) {
    let spawned = std::thread::Builder::new()
        .name("bcode-git-watch-retire".into())
        .spawn(move || drop(source));
    drop(spawned);
}

fn probe_target(probe_dir: &Path) -> WatchTarget {
    WatchTarget {
        path: probe_dir.to_owned(),
        kind: WatchKind::GitDir,
    }
}

/// A new private directory for probes, symbolic links resolved as native events name it.
fn probe_dir() -> Result<PathBuf, SourceError> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "bcode-git-probe-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).map_err(|error| SourceError(error.to_string()))?;
    Ok(std::fs::canonicalize(&dir).unwrap_or(dir))
}

const fn add(left: WatchCounters, right: WatchCounters) -> WatchCounters {
    WatchCounters {
        events: left.events + right.events,
        walks: left.walks + right.walks,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Native events seen, for the supervisor to wait on.
#[derive(Default)]
struct Pulse {
    counts: Mutex<PulseCounts>,
    changed: Condvar,
}

#[derive(Debug, Default, Clone, Copy)]
struct PulseCounts {
    /// Probe sightings.
    probes: u64,
    /// Other native events.
    events: u64,
}

impl Pulse {
    fn note(&self, probe: bool) {
        let mut counts = lock(&self.counts);
        if probe {
            counts.probes += 1;
        } else {
            counts.events += 1;
        }
        drop(counts);
        self.changed.notify_all();
    }

    fn counts(&self) -> PulseCounts {
        *lock(&self.counts)
    }

    /// Wait at most `timeout` for a probe sighting after `seen` of them.
    fn await_probe(&self, seen: u64, timeout: Duration) -> bool {
        self.changed
            .wait_timeout_while(lock(&self.counts), timeout, |counts| counts.probes <= seen)
            .unwrap_or_else(PoisonError::into_inner)
            .0
            .probes
            > seen
    }
}

/// Ends the supervisor's waits.
#[derive(Default)]
struct Stopper {
    stopped: Mutex<bool>,
    wake: Condvar,
}

impl Stopper {
    fn stop(&self) {
        *lock(&self.stopped) = true;
        self.wake.notify_all();
    }

    fn stopped(&self) -> bool {
        *lock(&self.stopped)
    }

    /// Wait `duration`; whether stopped meanwhile.
    fn sleep(&self, duration: Duration) -> bool {
        let stopped = lock(&self.stopped);
        let (stopped, _) = self
            .wake
            .wait_timeout_while(stopped, duration, |stopped| !*stopped)
            .unwrap_or_else(PoisonError::into_inner);
        *stopped
    }
}

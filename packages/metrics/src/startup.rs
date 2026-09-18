//! Opt-in, bounded startup diagnostics, independent of ordinary metric retention.
//!
//! Reports are diagnostic evidence, not execution authority. Schema 1 is the only
//! supported representation. Readers never repair or overwrite reports.

use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, mpsc};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const SLOTS: usize = 32;
const MAX_PHASES: usize = 1_024;
const MAX_BYTES: u64 = 512 * 1024;
static RECORDER: OnceLock<Recorder> = OnceLock::new();

/// One process's bounded startup timeline. Intervals use its own monotonic clock.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartupReport {
    /// Independently versioned diagnostic format; unknown versions are rejected.
    pub schema_version: u32,
    /// Process identity, not ownership evidence.
    pub pid: u32,
    /// Exact artifact's diagnostic identity.
    pub artifact: String,
    /// Launcher identifier inherited by the child, when available.
    pub correlation: Option<String>,
    /// Wall clock anchor for approximate cross-process alignment only.
    pub started_unix_us: u64,
    /// Process-local observation horizon.
    pub elapsed_us: u64,
    /// Time not covered by any measured interval (overlaps counted once).
    pub unattributed_us: u64,
    /// Events omitted because the bounded phase capacity was exhausted.
    pub dropped_phases: u64,
    /// Bounded low-cardinality workload counts; absent in older schema-1 reports.
    #[serde(default)]
    pub counts: std::collections::BTreeMap<String, u64>,
    /// A readiness milestone was observed; not a durable-resume guarantee.
    pub ready: bool,
    /// Bounded phase intervals; unfinished phases remain explicitly incomplete.
    pub phases: Vec<StartupPhase>,
}

/// A named operation interval; nesting/overlap is represented by source times.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartupPhase {
    /// Stable operation name, never request content or secrets.
    pub name: String,
    /// Validated plugin or diagnostic correlation identifier, never arbitrary text.
    #[serde(default)]
    pub subject: Option<String>,
    /// Offset from process entry.
    pub start_us: u64,
    /// End offset, absent when interrupted or still active.
    pub end_us: Option<u64>,
    /// `ok`, `error`, or `incomplete`; dropping a guard never implies success.
    pub outcome: String,
}

struct Recorder {
    started: Instant,
    sender: mpsc::SyncSender<Message>,
    next: std::sync::atomic::AtomicU64,
    ready: std::sync::atomic::AtomicBool,
}

enum Message {
    Start(u64, StartupPhase),
    End(u64, u64, &'static str),
    Count(&'static str, u64),
    Interval(u64, u64, u64),
    Ready(u64),
    Overflow,
    Flush(mpsc::SyncSender<()>),
}

/// An optional phase guard. No allocation or clock read occurs when disabled.
#[derive(Debug)]
pub struct Phase(Option<u64>);

impl Phase {
    /// Mark a successfully completed phase.
    pub fn finish(mut self) {
        self.end("ok");
    }

    /// Mark a failed phase without recording error text.
    pub fn fail(mut self) {
        self.end("error");
    }

    /// Mark a completed operation according to its result, without recording errors.
    pub fn finish_result<T, E>(mut self, result: &Result<T, E>) {
        self.end(if result.is_ok() { "ok" } else { "error" });
    }

    fn end(&mut self, outcome: &'static str) {
        if let Some(id) = self.0.take()
            && let Some(recorder) = RECORDER.get()
        {
            let _ = recorder.sender.try_send(Message::End(
                id,
                micros(recorder.started.elapsed()),
                outcome,
            ));
        }
    }
}

impl Drop for Phase {
    fn drop(&mut self) {
        self.end("incomplete");
    }
}

/// Time a synchronous fallible operation without retaining its value or error.
///
/// # Errors
/// Returns the operation's original error unchanged.
pub fn measure<T, E>(name: &'static str, operation: impl FnOnce() -> Result<T, E>) -> Result<T, E> {
    let phase = phase(name);
    let result = operation();
    phase.finish_result(&result);
    result
}

/// Start a low-cardinality phase. Names must be static, secret-safe operation names.
#[must_use]
pub fn phase(name: &'static str) -> Phase {
    phase_for(name, "")
}

/// Start a phase for a plugin ID or diagnostic launch ID. Invalid subjects are omitted.
#[must_use]
pub fn phase_for(name: &'static str, subject: &str) -> Phase {
    let Some(recorder) = RECORDER.get() else {
        return Phase(None);
    };
    if recorder.ready.load(std::sync::atomic::Ordering::Relaxed) {
        return Phase(None);
    }
    let id = recorder
        .next
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if id >= MAX_PHASES as u64 {
        recorder
            .ready
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = recorder.sender.try_send(Message::Overflow);
        return Phase(None);
    }
    let value = StartupPhase {
        name: name.to_owned(),
        subject: (!subject.is_empty()
            && subject.len() <= 128
            && subject
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_')))
        .then(|| subject.to_owned()),
        start_us: micros(recorder.started.elapsed()),
        end_us: None,
        outcome: "incomplete".to_owned(),
    };
    if recorder.sender.try_send(Message::Start(id, value)).is_err() {
        return Phase(None);
    }
    Phase(Some(id))
}

/// Record a bounded, secret-safe workload count while startup collection is active.
pub fn count(name: &'static str, value: u64) {
    if let Some(recorder) = RECORDER.get()
        && !recorder.ready.load(std::sync::atomic::Ordering::Relaxed)
    {
        let _ = recorder.sender.try_send(Message::Count(name, value));
    }
}

/// Record a completed pre-configuration interval after opt-in has been resolved.
/// Invalid or out-of-order intervals are ignored; names must be secret-safe constants.
pub fn completed_interval(name: &'static str, start: Instant, end: Instant) {
    let Some(recorder) = RECORDER.get() else {
        return;
    };
    let (Some(start), Some(end)) = (
        start.checked_duration_since(recorder.started),
        end.checked_duration_since(recorder.started),
    ) else {
        return;
    };
    if end < start {
        return;
    }
    let mut guard = phase(name);
    if let Some(id) = guard.0.take() {
        let _ = recorder
            .sender
            .try_send(Message::Interval(id, micros(start), micros(end)));
    }
}

/// Record a readiness milestone without redefining application readiness.
pub fn ready() {
    if let Some(recorder) = RECORDER.get() {
        recorder
            .ready
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = recorder
            .sender
            .try_send(Message::Ready(micros(recorder.started.elapsed())));
    }
}

/// Drain queued diagnostics at orderly CLI exit, waiting at most one second.
/// Returns false when storage failed or the worker did not acknowledge the flush.
#[must_use]
pub fn flush() -> bool {
    let Some(recorder) = RECORDER.get() else {
        return true;
    };
    let (sender, receiver) = mpsc::sync_channel(1);
    recorder.sender.try_send(Message::Flush(sender)).is_ok()
        && receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .is_ok()
}

fn micros(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

/// Initialize opt-in collection once for this process, after configuration resolution.
///
/// `started` must be captured at process/application entry. Early setup remains unattributed.
/// Storage is capped at 32 process reports of 512 KiB, plus fixed lock/temp files.
///
/// # Errors
/// Returns an error for unavailable/unsafe storage, exhausted active slots, or worker creation.
pub fn initialize(root: &Path, started: Instant, artifact: String) -> io::Result<()> {
    if RECORDER.get().is_some() {
        return Ok(());
    }
    if let Ok(metadata) = fs::symlink_metadata(root)
        && (!metadata.is_dir() || metadata.file_type().is_symlink())
    {
        return Err(io::Error::other("startup storage must be a real directory"));
    }
    fs::create_dir_all(root)?;
    let root = root.canonicalize()?;
    let first = std::process::id() as usize % SLOTS;
    let mut selected = None;
    for offset in 0..SLOTS {
        let slot = (first + offset) % SLOTS;
        let path = root.join(format!("{slot}.lock"));
        reject_symlink(&path)?;
        let lock = safe_options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        if lock.try_lock().is_ok() {
            selected = Some((slot, lock));
            break;
        }
    }
    let (slot, lock) =
        selected.ok_or_else(|| io::Error::other("all startup report slots are active"))?;
    let path = root.join(format!("{slot}.json"));
    reject_symlink(&path)?;
    // Do not replace future or corrupt diagnostic state implicitly.
    if path.exists() {
        let _ = read_report(&path)?;
    }
    let correlation = std::env::var("BCODE_STARTUP_CORRELATION").ok().filter(|s| {
        s.len() <= 64 && !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit() || b == b'-')
    });
    let report = StartupReport {
        schema_version: 1,
        pid: std::process::id(),
        artifact,
        correlation,
        started_unix_us: micros(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default(),
        )
        .saturating_sub(micros(started.elapsed())),
        elapsed_us: micros(started.elapsed()),
        unattributed_us: 0,
        dropped_phases: 0,
        counts: std::collections::BTreeMap::new(),
        ready: false,
        phases: Vec::new(),
    };
    persist(&path, &report)?;
    let (sender, receiver) = mpsc::sync_channel(4096);
    std::thread::Builder::new()
        .name("startup-report".to_owned())
        .spawn(move || {
            worker(&path, lock, report, &receiver);
        })?;
    let _ = RECORDER.set(Recorder {
        started,
        sender,
        next: std::sync::atomic::AtomicU64::new(0),
        ready: std::sync::atomic::AtomicBool::new(false),
    });
    Ok(())
}

fn safe_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW).mode(0o600);
    }
    options
}

fn reject_symlink(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if !meta.is_file() => Err(io::Error::other(
            "startup report path is not a regular file",
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn persist(path: &Path, report: &StartupReport) -> io::Result<()> {
    let temp = path.with_extension("tmp");
    reject_symlink(&temp)?;
    let bytes = serde_json::to_vec(report)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(io::Error::other("startup report exceeds byte budget"));
    }
    let mut file = safe_options()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temp)?;
    file.write_all(&bytes)?;
    fs::rename(temp, path)
}

fn worker(path: &Path, _lock: File, mut report: StartupReport, receiver: &mpsc::Receiver<Message>) {
    let mut ids: std::collections::BTreeMap<u64, usize> = std::collections::BTreeMap::new();
    let mut last_persist = Instant::now();
    let mut dirty = false;
    loop {
        let pending = if dirty {
            receiver.recv_timeout(std::time::Duration::from_millis(25))
        } else {
            receiver
                .recv()
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
        };
        let message = match pending {
            Ok(message) => message,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if dirty {
                    report.unattributed_us = unattributed(&report);
                    if persist(path, &report).is_err() {
                        eprintln!(
                            "bcode: startup diagnostic persistence failed; report may be incomplete"
                        );
                        return;
                    }
                    last_persist = Instant::now();
                    dirty = false;
                }
                continue;
            }
        };
        dirty = true;
        let urgent = matches!(&message, Message::Ready(_) | Message::Flush(_));
        match message {
            Message::Count(name, value) => {
                if report.counts.len() < 32 || report.counts.contains_key(name) {
                    let count = report.counts.entry(name.to_owned()).or_default();
                    *count = count.saturating_add(value);
                }
            }
            Message::Interval(id, start, end) => {
                if let Some(index) = ids.remove(&id) {
                    report.phases[index].start_us = start;
                    report.phases[index].end_us = Some(end);
                    "ok".clone_into(&mut report.phases[index].outcome);
                }
            }
            Message::Overflow => {
                report.dropped_phases += 1;
            }
            Message::Flush(sender) => {
                report.unattributed_us = unattributed(&report);
                if persist(path, &report).is_err() {
                    break;
                }
                last_persist = Instant::now();
                dirty = false;
                let _ = sender.try_send(());
                continue;
            }
            Message::Start(id, phase) => {
                report.elapsed_us = report.elapsed_us.max(phase.start_us);
                if report.phases.len() < MAX_PHASES {
                    ids.insert(id, report.phases.len());
                    report.phases.push(phase);
                } else {
                    report.dropped_phases += 1;
                }
            }
            Message::End(id, end, outcome) => {
                report.elapsed_us = report.elapsed_us.max(end);
                if let Some(index) = ids.remove(&id) {
                    report.phases[index].end_us = Some(end);
                    outcome.clone_into(&mut report.phases[index].outcome);
                }
            }
            Message::Ready(end) => {
                report.elapsed_us = report.elapsed_us.max(end);
                report.ready = true;
            }
        }
        if urgent || last_persist.elapsed() >= std::time::Duration::from_millis(25) {
            report.unattributed_us = unattributed(&report);
            if persist(path, &report).is_err() {
                eprintln!("bcode: startup diagnostic persistence failed; report may be incomplete");
                return;
            }
            last_persist = Instant::now();
            dirty = false;
        }
    }
    report.unattributed_us = unattributed(&report);
    if persist(path, &report).is_err() {
        eprintln!("bcode: startup diagnostic persistence failed; report may be incomplete");
    }
}

fn unattributed(report: &StartupReport) -> u64 {
    let mut intervals: Vec<_> = report
        .phases
        .iter()
        .filter_map(|p| p.end_us.map(|end| (p.start_us, end)))
        .collect();
    intervals.sort_unstable();
    let mut covered = 0;
    let mut last = 0;
    for (start, end) in intervals {
        covered += end.saturating_sub(start.max(last));
        last = last.max(end);
    }
    report.elapsed_us.saturating_sub(covered)
}

fn read_report(path: &Path) -> io::Result<StartupReport> {
    reject_symlink(path)?;
    let mut bytes = Vec::new();
    safe_options()
        .read(true)
        .open(path)?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(io::Error::other("startup report exceeds byte budget"));
    }
    let report: StartupReport = serde_json::from_slice(&bytes)?;
    if report.schema_version != 1 || report.phases.len() > MAX_PHASES {
        return Err(io::Error::other(
            "unsupported startup report representation",
        ));
    }
    Ok(report)
}

/// Read retained reports without starting a daemon, creating directories, or repairing state.
/// Reports are returned newest first. Reads inspect only the fixed 32 slots.
///
/// # Errors
/// Returns an error for unreadable, corrupt, oversized, unsafe, or future reports.
pub fn reports(root: &Path) -> io::Result<Vec<StartupReport>> {
    match fs::symlink_metadata(root) {
        Ok(meta) if !meta.is_dir() || meta.file_type().is_symlink() => {
            return Err(io::Error::other("startup storage must be a real directory"));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
        Ok(_) => (),
    }
    let mut reports = Vec::new();
    for slot in 0..SLOTS {
        let path: PathBuf = root.join(format!("{slot}.json"));
        match read_report(&path) {
            Ok(report) => reports.push(report),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error),
        }
    }
    reports.sort_by_key(|report| std::cmp::Reverse(report.started_unix_us));
    Ok(reports)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> StartupReport {
        StartupReport {
            schema_version: 1,
            pid: 1,
            artifact: "test".into(),
            correlation: None,
            started_unix_us: 0,
            elapsed_us: 100,
            unattributed_us: 0,
            dropped_phases: 0,
            counts: std::collections::BTreeMap::new(),
            ready: false,
            phases: Vec::new(),
        }
    }

    #[test]
    fn measured_operation_preserves_values_errors_and_runs_once() {
        let mut calls = 0;
        let result = measure("test.operation", || {
            calls += 1;
            Ok::<_, &str>(42)
        });
        assert_eq!(result, Ok(42));
        assert_eq!(calls, 1);
        assert_eq!(
            measure("test.failure", || Err::<(), _>("unchanged")),
            Err("unchanged")
        );
    }

    #[test]
    fn flush_acknowledges_persisted_batch_before_worker_exit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("0.json");
        let worker_path = path.clone();
        let lock = File::create(dir.path().join("0.lock")).unwrap();
        let (sender, receiver) = mpsc::sync_channel(8);
        let worker = std::thread::spawn(move || worker(&worker_path, lock, report(), &receiver));
        sender.send(Message::Count("observed", 7)).unwrap();
        let (ack, acknowledged) = mpsc::sync_channel(1);
        sender.send(Message::Flush(ack)).unwrap();
        acknowledged
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert_eq!(read_report(&path).unwrap().counts["observed"], 7);
        drop(sender);
        worker.join().unwrap();
    }

    #[test]
    fn overlaps_do_not_hide_unmeasured_time() {
        let mut report = report();
        for (start, end) in [(10, 60), (20, 40), (50, 80)] {
            report.phases.push(StartupPhase {
                name: "test".into(),
                subject: None,
                start_us: start,
                end_us: Some(end),
                outcome: "ok".into(),
            });
        }
        assert_eq!(unattributed(&report), 30);
    }

    #[test]
    fn bounded_reader_preserves_future_reports_and_missing_roots() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("absent");
        assert!(reports(&missing).unwrap().is_empty());
        assert!(!missing.exists());
        let mut report = report();
        report.schema_version = 2;
        let path = dir.path().join("0.json");
        persist(&path, &report).unwrap();
        let before = fs::read(&path).unwrap();
        assert!(reports(dir.path()).is_err());
        assert_eq!(fs::read(path).unwrap(), before);
    }

    #[test]
    fn rejects_oversized_reports() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("0.json"),
            vec![b' '; usize::try_from(MAX_BYTES).unwrap() + 1],
        )
        .unwrap();
        assert!(reports(dir.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn report_symlinks_cannot_escape_storage() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("0.json")).unwrap();
        assert!(reports(dir.path()).is_err());
    }

    #[test]
    fn worker_bounds_phase_retention_and_records_completion() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("0.json");
        let lock = File::create(dir.path().join("0.lock")).unwrap();
        let (sender, receiver) = mpsc::sync_channel(MAX_PHASES + 3);
        for id in 0..=MAX_PHASES as u64 {
            sender
                .send(Message::Start(
                    id,
                    StartupPhase {
                        name: "bounded".into(),
                        subject: None,
                        start_us: id,
                        end_us: None,
                        outcome: "incomplete".into(),
                    },
                ))
                .unwrap();
        }
        sender.send(Message::End(0, 20, "error")).unwrap();
        drop(sender);
        worker(&path, lock, report(), &receiver);
        let report = read_report(&path).unwrap();
        assert_eq!(report.phases.len(), MAX_PHASES);
        assert_eq!(report.dropped_phases, 1);
        assert_eq!(report.phases[0].outcome, "error");
        assert_eq!(report.phases[0].end_us, Some(20));
        assert!(report.phases[1].end_us.is_none());
        assert_eq!(report.phases[1].outcome, "incomplete");
    }

    #[test]
    fn worker_retains_early_intervals_counts_and_ready_marker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("0.json");
        let lock = File::create(dir.path().join("0.lock")).unwrap();
        let (sender, receiver) = mpsc::sync_channel(8);
        sender
            .send(Message::Start(
                1,
                StartupPhase {
                    name: "pending".into(),
                    subject: None,
                    start_us: 2,
                    end_us: None,
                    outcome: "incomplete".into(),
                },
            ))
            .unwrap();
        sender.send(Message::Count("runs", 3)).unwrap();
        sender.send(Message::Interval(1, 1, 50)).unwrap();
        sender.send(Message::Ready(100)).unwrap();
        drop(sender);
        worker(&path, lock, report(), &receiver);
        let reports = reports(dir.path()).unwrap();
        assert!(reports[0].ready);
        assert_eq!(reports[0].counts["runs"], 3);
        assert_eq!(reports[0].phases[0].start_us, 1);
        assert_eq!(reports[0].phases[0].end_us, Some(50));
    }
}

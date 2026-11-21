//! A guarded source falls back to polling while native events stop, and returns to them.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use bcode_git_live::source::{
    ChangeSource, EventSink, GuardTiming, GuardedSource, NativeFactory, RecursiveSource,
    SourceEvent, WatchHealth, WatchKind, WatchTarget,
};

const FAST: GuardTiming = GuardTiming {
    idle: Duration::from_millis(100),
    timeout: Duration::from_millis(400),
    attempts: 2,
    retry: Duration::from_millis(200),
    poll: Duration::from_millis(100),
};

/// Native sources whose events are dropped while `muted` holds, as when `fseventsd` wedges.
fn mutable_native(muted: &Arc<AtomicBool>) -> NativeFactory {
    let muted = muted.clone();
    Arc::new(move |sink: EventSink| {
        let muted = muted.clone();
        let filtered: EventSink = Arc::new(move |event| {
            if !muted.load(Ordering::SeqCst) {
                sink(event);
            }
        });
        Ok(Box::new(RecursiveSource::new(filtered)?) as Box<dyn ChangeSource>)
    })
}

fn worktree() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = std::fs::canonicalize(dir.path()).unwrap();
    let status = Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(&path)
        .status()
        .unwrap();
    assert!(status.success());
    (dir, path)
}

fn start(path: &Path, muted: &Arc<AtomicBool>) -> (GuardedSource, mpsc::Receiver<SourceEvent>) {
    let (sender, events) = mpsc::channel();
    let sink: EventSink = Arc::new(move |event| {
        let _ = sender.send(event);
    });
    let mut source = GuardedSource::new("Native", mutable_native(muted), sink, FAST).unwrap();
    source
        .watch(&WatchTarget {
            path: path.to_owned(),
            kind: WatchKind::Worktree {
                common_dir: path.join(".git"),
            },
        })
        .unwrap();
    (source, events)
}

/// Take events until one satisfies `wanted` (at most ten seconds).
fn expect(events: &mpsc::Receiver<SourceEvent>, what: &str, wanted: impl Fn(&SourceEvent) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(left) {
            Ok(event) if wanted(&event) => return,
            Ok(_) => {}
            Err(_) => panic!("timed out waiting for {what}"),
        }
    }
}

fn names(event: &SourceEvent, path: &Path) -> bool {
    matches!(event, SourceEvent::Changed(paths) if paths.iter().any(|changed| changed == path))
}

#[test]
fn silent_native_events_fall_back_to_polling_and_recover() {
    let (_dir, path) = worktree();
    let muted = Arc::new(AtomicBool::new(true));
    let (_source, events) = start(&path, &muted);

    expect(
        &events,
        "falling back to polling",
        |event| matches!(event, SourceEvent::Health(health) if health.is_polling()),
    );
    expect(&events, "a rescan after falling back", |event| {
        matches!(event, SourceEvent::Rescan(None))
    });
    let polled = path.join("polled.txt");
    std::fs::write(&polled, "1\n").unwrap();
    expect(&events, "the edit, found by polling", |event| {
        names(event, &polled)
    });

    // Native events flow again (as after `killall fseventsd`).
    muted.store(false, Ordering::SeqCst);
    expect(&events, "returning to native events", |event| {
        matches!(event, SourceEvent::Health(WatchHealth::Native))
    });
    expect(&events, "a rescan after returning", |event| {
        matches!(event, SourceEvent::Rescan(None))
    });
    let native = path.join("native.txt");
    std::fs::write(&native, "1\n").unwrap();
    expect(&events, "the edit, seen natively", |event| {
        names(event, &native)
    });
}

#[test]
fn working_native_events_stay_native_and_hide_probes() {
    let (_dir, path) = worktree();
    let muted = Arc::new(AtomicBool::new(false));
    let (_source, events) = start(&path, &muted);

    // Several idle checks probe meanwhile.
    let deadline = Instant::now() + Duration::from_millis(1500);
    while let Ok(event) = events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        assert!(
            !matches!(event, SourceEvent::Health(_)),
            "health changed: {event:?}"
        );
        if let SourceEvent::Changed(paths) = &event {
            assert!(
                paths.iter().all(|changed| changed.starts_with(&path)),
                "a probe leaked: {paths:?}"
            );
        }
    }
    let edited = path.join("edited.txt");
    std::fs::write(&edited, "1\n").unwrap();
    expect(&events, "the edit", |event| names(event, &edited));
}

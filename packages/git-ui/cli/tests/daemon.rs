//! `bcode-git` clients start one shared daemon, share it, and it exits once idle.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use bcode_git::models::LineStats;
use bcode_git::{CommandLog, Repository};
use bcode_git_daemon::{ClientOptions, DaemonSession, Launch};
use bcode_git_live::source::SourceKind;
use bcode_git_live::{CountsMode, EngineConfig, LineCounts, LiveUpdate};

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn wait_for(what: &str, updates: &mpsc::Receiver<LiveUpdate>, done: impl Fn(&LiveUpdate) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "timed out waiting for {what}");
        if let Ok(update) = updates.recv_timeout(left)
            && done(&update)
        {
            return;
        }
    }
}

fn counted(update: &LiveUpdate, path: &str, stats: LineStats) -> bool {
    update.primary.as_ref().is_some_and(|primary| {
        matches!(&primary.counts, LineCounts::Total(total) if total.get(path) == Some(&stats))
    })
}

fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[test]
fn clients_start_one_daemon_share_it_and_it_exits_when_idle() {
    let repo_dir = tempfile::tempdir().unwrap();
    let path = repo_dir.path();
    git(path, &["init", "--quiet", "--initial-branch", "main"]);
    git(path, &["config", "user.email", "test@example.com"]);
    git(path, &["config", "user.name", "Test"]);
    std::fs::write(path.join("a.txt"), "1\n").unwrap();
    git(path, &["add", "--all"]);
    git(
        path,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "init",
        ],
    );

    let state = tempfile::tempdir().unwrap();
    let socket = bcode_git_daemon::socket_path(state.path());
    let options = ClientOptions::new(
        socket.clone(),
        Some(Launch {
            program: PathBuf::from(env!("CARGO_BIN_EXE_bcode-git")),
            args: vec![
                "--git-daemon-idle-ms".into(),
                "500".into(),
                bcode_git_ui::DAEMON_ARGUMENT.into(),
            ],
        }),
    );
    let config = EngineConfig {
        counts: CountsMode::Total,
        ..EngineConfig::default()
    };
    // Two clients starting at the same moment end up with one daemon.
    let connect = || {
        let options = options.clone();
        let path = path.to_owned();
        std::thread::spawn(move || {
            let repo = Repository::discover(&path, CommandLog::default()).unwrap();
            let (sender, updates) = mpsc::channel();
            let session = DaemonSession::connect(
                &options,
                &repo,
                config,
                Some(SourceKind::Auto),
                Arc::new(move |update| {
                    let _ = sender.send(update);
                }),
            )
            .unwrap();
            (session, updates)
        })
    };
    let (first, second) = (connect(), connect());
    let (first, first_updates) = first.join().unwrap();
    let (second, second_updates) = second.join().unwrap();
    assert_eq!(first.daemon_pid(), second.daemon_pid());
    let pid = first.daemon_pid();
    assert_ne!(pid, std::process::id());

    std::fs::write(path.join("a.txt"), "1\n2\n").unwrap();
    wait_for("the first client's update", &first_updates, |update| {
        counted(update, "a.txt", LineStats::text(1, 0))
    });
    wait_for("the second client's update", &second_updates, |update| {
        counted(update, "a.txt", LineStats::text(1, 0))
    });
    assert!(first.served_by_daemon() && second.served_by_daemon());

    drop((first, second));
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive(pid) || socket.exists() {
        assert!(Instant::now() < deadline, "the idle daemon did not exit");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn stop_daemon_stops_the_shared_daemon_and_clients_carry_on() {
    let repo_dir = tempfile::tempdir().unwrap();
    let path = repo_dir.path();
    git(path, &["init", "--quiet", "--initial-branch", "main"]);
    git(path, &["config", "user.email", "test@example.com"]);
    git(path, &["config", "user.name", "Test"]);
    std::fs::write(path.join("a.txt"), "1\n").unwrap();
    git(path, &["add", "--all"]);
    git(
        path,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "init",
        ],
    );

    // The binary's state directory is `$XDG_STATE_HOME/bcode-git`.
    let xdg = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    let state = xdg.path().join("bcode-git");
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_bcode-git"));
    let options = ClientOptions::new(
        bcode_git_daemon::socket_path(&state),
        Some(Launch {
            program: binary.clone(),
            args: vec![bcode_git_ui::DAEMON_ARGUMENT.into()],
        }),
    );
    let repo = Repository::discover(path, CommandLog::default()).unwrap();
    let (sender, updates) = mpsc::channel();
    let session = DaemonSession::connect(
        &options,
        &repo,
        EngineConfig {
            counts: CountsMode::Total,
            ..EngineConfig::default()
        },
        Some(SourceKind::Auto),
        Arc::new(move |update| {
            let _ = sender.send(update);
        }),
    )
    .unwrap();
    let pid = session.daemon_pid();
    assert!(alive(pid));

    let output = Command::new(&binary)
        .arg("--stop-daemon")
        .env("XDG_STATE_HOME", xdg.path())
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{text}");
    assert!(
        text.contains(&format!("daemon {pid}")) && text.contains("stopped"),
        "{text}"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(pid) || session.served_by_daemon() {
        assert!(Instant::now() < deadline, "the daemon did not stop");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::fs::write(path.join("a.txt"), "1\n2\n").unwrap();
    wait_for("the edit, served in-process", &updates, |update| {
        counted(update, "a.txt", LineStats::text(1, 0))
    });

    let output = Command::new(&binary)
        .arg("--stop-daemon")
        .env("XDG_STATE_HOME", xdg.path())
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&output.stdout).contains("no bcode-git daemon is running"));
}

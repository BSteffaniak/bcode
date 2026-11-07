//! The daemon serves clients of a repository together, and clients outlive it.
#![cfg(unix)]

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use bcode_git::models::LineStats;
use bcode_git::{CommandLog, Repository};
use bcode_git_daemon::protocol::{self, ClientMessage, ServerMessage};
use bcode_git_daemon::server::{ServeOptions, Server};
use bcode_git_daemon::{ClientOptions, DaemonSession};
use bcode_git_live::source::SourceKind;
use bcode_git_live::{
    CountsMode, EngineConfig, EngineStats, LineCounts, LiveSession, LiveUpdate, WorktreeSnapshot,
};
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}");
}

fn repository() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
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
    dir
}

const CONFIG: EngineConfig = EngineConfig {
    counts: CountsMode::Total,
    other_worktrees: false,
    max_other_worktrees: 16,
    scanner: bcode_git_live::scan::ScannerKind::Plain,
};

/// A client and what it was told.
struct Client {
    session: DaemonSession,
    updates: mpsc::Receiver<LiveUpdate>,
    primary: Option<WorktreeSnapshot>,
    stats: EngineStats,
}

impl Client {
    fn connect(socket: &Path, path: &Path) -> Self {
        let repo = Repository::discover(path, CommandLog::default()).unwrap();
        let (sender, updates) = mpsc::channel();
        let session = DaemonSession::connect(
            &ClientOptions::new(socket.to_owned(), None),
            &repo,
            CONFIG,
            Some(SourceKind::Auto),
            Arc::new(move |update| {
                let _ = sender.send(update);
            }),
        )
        .unwrap();
        let mut client = Self {
            session,
            updates,
            primary: None,
            stats: EngineStats::default(),
        };
        client.wait("everything known", |client| client.primary.is_some());
        client
    }

    fn wait(&mut self, what: &str, done: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done(self) {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "timed out waiting for {what}");
            if let Ok(update) = self.updates.recv_timeout(left) {
                assert!(update.errors.is_empty(), "{:?}", update.errors);
                self.stats = update.stats;
                if update.primary.is_some() {
                    self.primary = update.primary;
                }
            }
        }
    }

    fn count(&self, path: &str) -> Option<LineStats> {
        match &self.primary.as_ref()?.counts {
            LineCounts::Total(total) => total.get(path).copied(),
            _ => None,
        }
    }
}

fn socket(dir: &TempDir) -> std::path::PathBuf {
    dir.path().join("daemon").join("test.sock")
}

#[test]
fn clients_of_a_repository_share_one_scan_and_each_check() {
    let state = tempfile::tempdir().unwrap();
    let server = Server::spawn(ServeOptions::new(socket(&state))).unwrap();
    let repo = repository();
    let mut first = Client::connect(&socket(&state), repo.path());
    let mut second = Client::connect(&socket(&state), repo.path());
    assert!(first.session.watching());
    // The second client was served what the first one's scan found.
    assert_eq!(second.stats.full_scans, 1);

    std::fs::write(repo.path().join("a.txt"), "1\n2\n").unwrap();
    first.wait("the edit", |client| {
        client.count("a.txt") == Some(LineStats::text(1, 0))
    });
    second.wait("the edit", |client| {
        client.count("a.txt") == Some(LineStats::text(1, 0))
    });
    assert_eq!(second.stats.full_scans, 1);
    assert_eq!(second.stats.path_checks, 1);
    // Requests reach the shared state too (a scan that finds nothing new sends nothing).
    second.session.request(bcode_git_live::Request::Full);
    std::thread::sleep(Duration::from_millis(300));
    std::fs::write(repo.path().join("a.txt"), "1\n2\n3\n").unwrap();
    first.wait("the next edit", |client| {
        client.count("a.txt") == Some(LineStats::text(2, 0))
    });
    assert_eq!(first.stats.full_scans, 2);
    server.shutdown();
}

#[test]
fn a_client_coming_back_is_served_without_scanning() {
    let state = tempfile::tempdir().unwrap();
    let server = Server::spawn(ServeOptions::new(socket(&state))).unwrap();
    let repo = repository();
    drop(Client::connect(&socket(&state), repo.path()));
    std::fs::write(repo.path().join("a.txt"), "1\n2\n").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let client = Client::connect(&socket(&state), repo.path());
    assert_eq!(client.count("a.txt"), Some(LineStats::text(1, 0)));
    assert_eq!(client.stats.full_scans, 1);
    server.shutdown();
}

#[test]
fn clients_serve_themselves_once_the_daemon_is_gone() {
    let state = tempfile::tempdir().unwrap();
    let server = Server::spawn(ServeOptions::new(socket(&state))).unwrap();
    let repo = repository();
    let mut client = Client::connect(&socket(&state), repo.path());
    assert!(client.session.served_by_daemon());

    server.shutdown();
    let deadline = Instant::now() + Duration::from_secs(5);
    while client.session.served_by_daemon() {
        assert!(Instant::now() < deadline, "still served by the daemon");
        std::thread::sleep(Duration::from_millis(10));
    }
    std::fs::write(repo.path().join("a.txt"), "").unwrap();
    client.wait("the edit, served in-process", |client| {
        client.count("a.txt") == Some(LineStats::text(0, 1))
    });
}

#[test]
fn another_protocol_or_an_unknown_worktree_is_refused() {
    let state = tempfile::tempdir().unwrap();
    let _server = Server::spawn(ServeOptions::new(socket(&state))).unwrap();
    let converse = |messages: &[ClientMessage]| {
        let stream = UnixStream::connect(socket(&state)).unwrap();
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);
        let mut answers = Vec::new();
        for message in messages {
            protocol::write(&mut writer, message).unwrap();
            answers.push(protocol::read::<ServerMessage>(&mut reader).unwrap());
        }
        answers
    };
    let answers = converse(&[ClientMessage::Hello { protocol: 999 }]);
    assert!(
        matches!(&answers[0], Some(ServerMessage::Refused { .. })),
        "{answers:?}"
    );
    let outside = tempfile::tempdir().unwrap();
    let answers = converse(&[
        ClientMessage::Hello {
            protocol: protocol::PROTOCOL,
        },
        ClientMessage::Subscribe {
            path: outside.path().to_owned(),
            config: CONFIG,
            watcher: None,
        },
    ]);
    assert!(
        matches!(&answers[1], Some(ServerMessage::Failed { .. })),
        "{answers:?}"
    );
}

#[test]
fn an_idle_daemon_exits_and_removes_its_socket() {
    let state = tempfile::tempdir().unwrap();
    let options = ServeOptions {
        idle: Duration::from_millis(300),
        linger: Duration::from_millis(100),
        ..ServeOptions::new(socket(&state))
    };
    let repo = repository();
    let server = Server::spawn(options).unwrap();
    let client = Client::connect(&socket(&state), repo.path());
    // Not while a client is connected.
    std::thread::sleep(Duration::from_millis(600));
    assert!(!server.finished());
    drop(client);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !server.finished() {
        assert!(Instant::now() < deadline, "the daemon did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!socket(&state).exists());
}

#[test]
fn stopping_asks_the_daemon_and_its_clients_carry_on() {
    let state = tempfile::tempdir().unwrap();
    let repo = repository();
    let server = Server::spawn(ServeOptions::new(socket(&state))).unwrap();
    let mut client = Client::connect(&socket(&state), repo.path());

    let stopped = bcode_git_daemon::control::stop_all(state.path());
    assert_eq!(stopped.len(), 1, "{stopped:?}");
    assert_eq!(stopped[0].1, bcode_git_daemon::control::Outcome::Stopped);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !server.finished() || client.session.served_by_daemon() {
        assert!(Instant::now() < deadline, "the daemon did not stop");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!socket(&state).exists());
    std::fs::write(repo.path().join("a.txt"), "").unwrap();
    client.wait("the edit, served in-process", |client| {
        client.count("a.txt") == Some(LineStats::text(0, 1))
    });
}

#[test]
fn a_daemon_that_ignores_being_asked_is_terminated() {
    use std::os::unix::net::UnixListener;
    let state = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(socket(&state).parent().unwrap()).unwrap();
    // An older (or stuck) daemon: it says hello but does not understand being asked to stop.
    let process = Command::new("sleep").arg("60").spawn().unwrap();
    let pid = process.id();
    let (exited, exits) = mpsc::channel();
    std::thread::spawn(move || {
        let mut process = process;
        let _ = exited.send(process.wait());
    });
    let listener = UnixListener::bind(socket(&state)).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            if let Ok(Some(ClientMessage::Hello { .. })) = protocol::read(&mut reader) {
                let _ = protocol::write(
                    &mut writer,
                    &ServerMessage::Welcome {
                        protocol: protocol::PROTOCOL,
                        pid,
                    },
                );
                let _ = protocol::read::<ClientMessage>(&mut reader);
            }
        }
    });

    let stopped = bcode_git_daemon::control::stop_all(state.path());
    assert_eq!(stopped.len(), 1, "{stopped:?}");
    assert_eq!(stopped[0].0.pid, Some(pid));
    assert_eq!(stopped[0].1, bcode_git_daemon::control::Outcome::Terminated);
    assert!(exits.recv_timeout(Duration::from_secs(5)).is_ok());
}

#[test]
fn sockets_nobody_answers_on_are_removed() {
    let state = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(socket(&state).parent().unwrap()).unwrap();
    drop(std::os::unix::net::UnixListener::bind(socket(&state)).unwrap());
    assert!(socket(&state).exists());
    let stopped = bcode_git_daemon::control::stop_all(state.path());
    assert_eq!(stopped.len(), 1, "{stopped:?}");
    assert_eq!(stopped[0].0.pid, None);
    assert_eq!(stopped[0].1, bcode_git_daemon::control::Outcome::Stale);
    assert!(!socket(&state).exists());
    // Nothing left to stop.
    assert!(bcode_git_daemon::control::stop_all(state.path()).is_empty());
}

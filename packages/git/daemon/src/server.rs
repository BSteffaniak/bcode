//! The daemon: serves live state for every repository its clients show.

use std::collections::BTreeMap;
use std::io::BufReader;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::{Duration, Instant};

use bcode_git::{CommandLog, Repository};
use bcode_git_live::{Hub, UpdateSink};

use crate::protocol::{self, ClientMessage, PROTOCOL, ServerMessage};

/// How a daemon runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeOptions {
    /// Socket to listen on.
    pub socket: PathBuf,
    /// Exit after this long without clients.
    pub idle: Duration,
    /// Stop serving a repository this long after its last client left (so a client that
    /// restarts finds it warm).
    pub linger: Duration,
}

impl ServeOptions {
    /// Listen on `socket`, exiting after a minute without clients and keeping a repository
    /// for half a minute after its last client.
    #[must_use]
    pub const fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            idle: Duration::from_secs(60),
            linger: Duration::from_secs(30),
        }
    }
}

struct Served {
    hub: Arc<Hub>,
    clients: usize,
    /// When its last client left.
    vacant_since: Option<Instant>,
}

#[derive(Default)]
struct State {
    connections: BTreeMap<u64, UnixStream>,
    next: u64,
    repositories: BTreeMap<PathBuf, Served>,
    /// When the last connection closed (or the daemon started).
    idle_since: Option<Instant>,
    stopping: bool,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    socket: PathBuf,
    /// Inode of the socket file this daemon created, to remove only that one.
    inode: u64,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Stop accepting: wake the listener with a connection of our own.
    fn stop(&self, state: &mut State) {
        state.stopping = true;
        for connection in state.connections.values() {
            let _ = connection.shutdown(std::net::Shutdown::Both);
        }
        let _ = UnixStream::connect(&self.socket);
        self.changed.notify_all();
    }
}

/// Serve until idle. Returns at once when another daemon already listens on the socket.
///
/// # Errors
/// * [`std::io::Error`] when the socket cannot be created.
pub fn serve(options: &ServeOptions) -> std::io::Result<()> {
    if let Some((listener, inode)) = bind(&options.socket)? {
        run(listener, options, &shared(options, inode));
    }
    Ok(())
}

/// A daemon running on a thread of this process, for embedding and tests.
pub struct Server {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    /// Start serving on a thread.
    ///
    /// # Errors
    /// * [`std::io::Error`] when the socket cannot be created or is already served.
    pub fn spawn(options: ServeOptions) -> std::io::Result<Self> {
        let (listener, inode) = bind(&options.socket)?.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::AddrInUse, "a daemon already listens")
        })?;
        let shared = shared(&options, inode);
        let thread = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("bcode-git-daemon".into())
                .spawn(move || run(listener, &options, &shared))?
        };
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// Whether the daemon has stopped (for example after going idle).
    #[must_use]
    pub fn finished(&self) -> bool {
        self.thread
            .as_ref()
            .is_none_or(std::thread::JoinHandle::is_finished)
    }

    /// Stop now, disconnecting every client.
    pub fn shutdown(mut self) {
        self.stop_and_join();
    }

    fn stop_and_join(&mut self) {
        {
            let mut state = self.shared.lock();
            if !state.stopping {
                self.shared.stop(&mut state);
            }
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

fn shared(options: &ServeOptions, inode: u64) -> Arc<Shared> {
    Arc::new(Shared {
        state: Mutex::new(State {
            idle_since: Some(Instant::now()),
            ..State::default()
        }),
        changed: Condvar::new(),
        socket: options.socket.clone(),
        inode,
    })
}

/// Listen on `socket`, replacing a stale one: the listener and the socket file's inode;
/// `None` when a daemon already answers there.
fn bind(socket: &Path) -> std::io::Result<Option<(UnixListener, u64)>> {
    crate::paths::private_directory(socket)?;
    if UnixStream::connect(socket).is_ok() {
        return Ok(None);
    }
    match std::fs::remove_file(socket) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = UnixListener::bind(socket)?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    let inode = std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(socket)?);
    Ok(Some((listener, inode)))
}

fn run(listener: UnixListener, options: &ServeOptions, shared: &Arc<Shared>) {
    let keeper = {
        let shared = shared.clone();
        let options = options.clone();
        std::thread::spawn(move || keep(&shared, &options))
    };
    for stream in listener.incoming() {
        let mut state = shared.lock();
        if state.stopping {
            break;
        }
        let Ok(stream) = stream else {
            continue;
        };
        let Ok(registered) = stream.try_clone() else {
            continue;
        };
        let id = state.next;
        state.next += 1;
        state.connections.insert(id, registered);
        state.idle_since = None;
        drop(state);
        let shared = shared.clone();
        let _ = std::thread::Builder::new()
            .name("bcode-git-daemon-client".into())
            .spawn(move || {
                let _ = converse(&stream, &shared);
                let mut state = shared.lock();
                state.connections.remove(&id);
                if state.connections.is_empty() {
                    state.idle_since = Some(Instant::now());
                }
                drop(state);
                shared.changed.notify_all();
            });
    }
    drop(listener);
    {
        let mut state = shared.lock();
        if !state.stopping {
            shared.stop(&mut state);
        }
        state.repositories.clear();
    }
    let _ = keeper.join();
    // Remove the socket only if it is still the file this daemon created (a newer daemon may
    // have replaced it). Asking whether anything answers there is no test: a git process
    // being started holds the listener for a moment.
    let ours = std::fs::metadata(&shared.socket)
        .is_ok_and(|metadata| std::os::unix::fs::MetadataExt::ino(&metadata) == shared.inode);
    if ours {
        let _ = std::fs::remove_file(&shared.socket);
    }
}

/// Drop repositories nobody shows any more and stop when idle.
fn keep(shared: &Shared, options: &ServeOptions) {
    let mut state = shared.lock();
    loop {
        if state.stopping {
            return;
        }
        let now = Instant::now();
        state.repositories.retain(|_, served| {
            served
                .vacant_since
                .is_none_or(|since| now.duration_since(since) < options.linger)
        });
        if let Some(since) = state.idle_since
            && state.connections.is_empty()
            && now.duration_since(since) >= options.idle
        {
            shared.stop(&mut state);
            return;
        }
        let next = state
            .repositories
            .values()
            .filter_map(|served| served.vacant_since.map(|since| since + options.linger))
            .chain(state.idle_since.map(|since| since + options.idle))
            .min()
            .map_or(Duration::from_secs(3600), |at| {
                at.saturating_duration_since(now) + Duration::from_millis(1)
            });
        state = shared
            .changed
            .wait_timeout(state, next)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
    }
}

/// One client's conversation.
fn converse(stream: &UnixStream, shared: &Shared) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream.try_clone()?;
    match protocol::read::<ClientMessage>(&mut reader)? {
        Some(ClientMessage::Hello { protocol }) if protocol == PROTOCOL => {
            protocol::write(
                &mut writer,
                &ServerMessage::Welcome {
                    protocol: PROTOCOL,
                    pid: std::process::id(),
                },
            )?;
        }
        Some(ClientMessage::Hello { protocol }) => {
            return protocol::write(
                &mut writer,
                &ServerMessage::Refused {
                    reason: format!("protocol {protocol} is not {PROTOCOL}"),
                },
            );
        }
        _ => return Ok(()),
    }
    let (path, config, watcher) = match protocol::read::<ClientMessage>(&mut reader)? {
        Some(ClientMessage::Subscribe {
            path,
            config,
            watcher,
        }) => (path, config, watcher),
        Some(ClientMessage::Stop) => {
            protocol::write(&mut writer, &ServerMessage::Stopping)?;
            let mut state = shared.lock();
            shared.stop(&mut state);
            return Ok(());
        }
        _ => return Ok(()),
    };
    let repo = match Repository::discover(&path, CommandLog::default()) {
        Ok(repo) => repo,
        Err(error) => {
            return protocol::write(
                &mut writer,
                &ServerMessage::Failed {
                    reason: error.message(),
                },
            );
        }
    };
    let key = std::fs::canonicalize(&repo.paths().common_dir)
        .unwrap_or_else(|_| repo.paths().common_dir.clone());
    let mut state = shared.lock();
    let served = state
        .repositories
        .entry(key.clone())
        .or_insert_with(|| Served {
            hub: Arc::new(Hub::start(&repo, config.scanner, watcher)),
            clients: 0,
            vacant_since: None,
        });
    served.clients += 1;
    served.vacant_since = None;
    let hub = served.hub.clone();
    drop(state);
    // Updates go out from a thread of their own, in order, after the acknowledgement.
    let (outgoing, queued) = mpsc::channel::<ServerMessage>();
    let _ = outgoing.send(ServerMessage::Subscribed {
        watching: hub.watching(),
    });
    let _ = std::thread::Builder::new()
        .name("bcode-git-daemon-writer".into())
        .spawn(move || {
            for message in queued {
                if protocol::write(&mut writer, &message).is_err() {
                    break;
                }
            }
        });
    let sink: UpdateSink = Arc::new(move |update| {
        let _ = outgoing.send(ServerMessage::Update {
            update: Box::new(update),
        });
    });
    let id = hub.subscribe(&repo, config, sink);
    while let Ok(Some(message)) = protocol::read::<ClientMessage>(&mut reader) {
        if let ClientMessage::Request { request } = message {
            hub.request(id, request);
        }
    }
    hub.unsubscribe(id);
    let mut state = shared.lock();
    if let Some(served) = state.repositories.get_mut(&key) {
        served.clients = served.clients.saturating_sub(1);
        if served.clients == 0 {
            served.vacant_since = Some(Instant::now());
        }
    }
    drop(state);
    shared.changed.notify_all();
    Ok(())
}

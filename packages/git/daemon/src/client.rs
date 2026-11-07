//! Clients of the daemon.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

#[cfg(not(unix))]
use bcode_git::Repository;
#[cfg(not(unix))]
use bcode_git_live::source::SourceKind;
#[cfg(not(unix))]
use bcode_git_live::{EngineConfig, LiveSession, Request, UpdateSink};

/// How to start the daemon: `program` with `args`, then the socket path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    /// Executable.
    pub program: PathBuf,
    /// Arguments before the socket path.
    pub args: Vec<OsString>,
}

impl Launch {
    /// The running executable with `args`, which must make it serve the socket path that
    /// follows them.
    ///
    /// # Errors
    /// * [`std::io::Error`] when the running executable cannot be found.
    pub fn current_exe(
        args: impl IntoIterator<Item = impl Into<OsString>>,
    ) -> std::io::Result<Self> {
        Ok(Self {
            program: std::env::current_exe()?,
            args: args.into_iter().map(Into::into).collect(),
        })
    }
}

/// How a client reaches the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientOptions {
    /// The daemon's socket (see [`crate::socket_path`]).
    pub socket: PathBuf,
    /// How to start the daemon when none answers; `None` only connects.
    pub launch: Option<Launch>,
    /// Longest wait for a started daemon to answer.
    pub start_timeout: Duration,
}

impl ClientOptions {
    /// Reach the daemon at `socket`, starting it with `launch` when needed.
    #[must_use]
    pub const fn new(socket: PathBuf, launch: Option<Launch>) -> Self {
        Self {
            socket,
            launch,
            start_timeout: Duration::from_secs(5),
        }
    }
}

/// Failure to be served by the daemon. The caller serves itself instead.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// No daemon answers and none could be started.
    #[error("the bcode-git daemon is unavailable: {0}")]
    Unavailable(String),
    /// The daemon speaks another protocol.
    #[error("the bcode-git daemon refused the client: {0}")]
    Refused(String),
    /// The daemon cannot serve this worktree.
    #[error("the bcode-git daemon cannot serve this worktree: {0}")]
    Failed(String),
    /// Talking to the daemon failed.
    #[error("talking to the bcode-git daemon failed: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(unix)]
pub use unix::DaemonSession;

/// Live state served by the daemon. Daemons need Unix domain sockets, so here connecting
/// always fails and callers serve themselves.
#[cfg(not(unix))]
pub struct DaemonSession(std::convert::Infallible);

#[cfg(not(unix))]
impl DaemonSession {
    /// Always [`ConnectError::Unavailable`] on this platform.
    ///
    /// # Errors
    /// * [`ConnectError::Unavailable`] always.
    pub fn connect(
        _options: &ClientOptions,
        _repo: &Repository,
        _config: EngineConfig,
        _watcher: Option<SourceKind>,
        _sink: UpdateSink,
    ) -> Result<Self, ConnectError> {
        Err(ConnectError::Unavailable(
            "daemons need Unix domain sockets".into(),
        ))
    }
}

#[cfg(not(unix))]
impl LiveSession for DaemonSession {
    fn request(&self, _request: Request) {
        match self.0 {}
    }

    fn watching(&self) -> bool {
        match self.0 {}
    }
}

#[cfg(unix)]
mod unix {
    use std::io::BufReader;
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::CommandExt as _;
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::{Duration, Instant};

    use bcode_git::Repository;
    use bcode_git_live::source::SourceKind;
    use bcode_git_live::{EngineConfig, LiveSession, LocalSession, Request, UpdateSink};

    use super::{ClientOptions, ConnectError};
    use crate::protocol::{self, ClientMessage, PROTOCOL, ServerMessage};

    /// Longest wait for the daemon's answers while connecting.
    const HANDSHAKE: Duration = Duration::from_secs(5);

    enum Link {
        Daemon(UnixStream),
        Local(LocalSession),
    }

    /// Live state for one client, served by the daemon. Should the daemon go away, the
    /// session serves itself in-process from then on (starting with a full scan), so the
    /// client never stops updating.
    pub struct DaemonSession {
        link: Arc<Mutex<Link>>,
        watching: bool,
        pid: u32,
    }

    impl DaemonSession {
        /// Be served the worktree of `repo` as `config` asks, reporting to `sink`.
        /// `watcher` is how the daemon watches the repository if nobody showed it yet
        /// (`None`: without watching). Starts the daemon when none answers and
        /// `options.launch` says how.
        ///
        /// # Errors
        /// * [`ConnectError`] when the daemon cannot serve the client; serve it in-process
        ///   instead.
        pub fn connect(
            options: &ClientOptions,
            repo: &Repository,
            config: EngineConfig,
            watcher: Option<SourceKind>,
            sink: UpdateSink,
        ) -> Result<Self, ConnectError> {
            let stream = connect_or_start(options)?;
            stream.set_read_timeout(Some(HANDSHAKE))?;
            let mut reader = BufReader::new(stream.try_clone()?);
            let mut writer = stream.try_clone()?;
            protocol::write(&mut writer, &ClientMessage::Hello { protocol: PROTOCOL })?;
            let pid = match protocol::read::<ServerMessage>(&mut reader)? {
                Some(ServerMessage::Welcome { pid, .. }) => pid,
                Some(ServerMessage::Refused { reason }) => {
                    return Err(ConnectError::Refused(reason));
                }
                other => {
                    return Err(ConnectError::Unavailable(format!(
                        "unexpected answer {other:?}"
                    )));
                }
            };
            protocol::write(
                &mut writer,
                &ClientMessage::Subscribe {
                    path: repo.root().to_owned(),
                    config,
                    watcher,
                },
            )?;
            let watching = match protocol::read::<ServerMessage>(&mut reader)? {
                Some(ServerMessage::Subscribed { watching }) => watching,
                Some(ServerMessage::Failed { reason }) => return Err(ConnectError::Failed(reason)),
                other => {
                    return Err(ConnectError::Unavailable(format!(
                        "unexpected answer {other:?}"
                    )));
                }
            };
            stream.set_read_timeout(None)?;
            let link = Arc::new(Mutex::new(Link::Daemon(writer)));
            let fallback = Fallback {
                link: link.clone(),
                repo: repo.clone(),
                config,
                watcher,
                sink: sink.clone(),
            };
            std::thread::Builder::new()
                .name("bcode-git-daemon-updates".into())
                .spawn(move || {
                    while let Ok(Some(message)) = protocol::read::<ServerMessage>(&mut reader) {
                        if let ServerMessage::Update { update } = message {
                            sink(*update);
                        }
                    }
                    fallback.serve_locally();
                })?;
            Ok(Self {
                link,
                watching,
                pid,
            })
        }

        /// Process id of the daemon that served (or still serves) this session.
        #[must_use]
        pub const fn daemon_pid(&self) -> u32 {
            self.pid
        }

        /// Whether the daemon still serves this session (rather than the session itself).
        #[must_use]
        pub fn served_by_daemon(&self) -> bool {
            matches!(
                *self.link.lock().unwrap_or_else(PoisonError::into_inner),
                Link::Daemon(_)
            )
        }
    }

    impl LiveSession for DaemonSession {
        fn request(&self, request: Request) {
            match &mut *self.link.lock().unwrap_or_else(PoisonError::into_inner) {
                // A failed write means the daemon went away; the update thread falls back.
                Link::Daemon(stream) => {
                    let _ = protocol::write(stream, &ClientMessage::Request { request });
                }
                Link::Local(session) => session.request(request),
            }
        }

        fn watching(&self) -> bool {
            self.watching
        }
    }

    impl Drop for DaemonSession {
        fn drop(&mut self) {
            if let Link::Daemon(stream) = &*self.link.lock().unwrap_or_else(PoisonError::into_inner)
            {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        }
    }

    /// What it takes to serve a session in-process once the daemon is gone.
    struct Fallback {
        link: Arc<Mutex<Link>>,
        repo: Repository,
        config: EngineConfig,
        watcher: Option<SourceKind>,
        sink: UpdateSink,
    }

    impl Fallback {
        fn serve_locally(self) {
            // The session itself may be gone (it shut the stream down): then the link has
            // no other owner and there is nobody to serve.
            if Arc::strong_count(&self.link) == 1 {
                return;
            }
            let mut link = self.link.lock().unwrap_or_else(PoisonError::into_inner);
            if matches!(*link, Link::Daemon(_)) {
                *link = Link::Local(LocalSession::start(
                    &self.repo,
                    self.config,
                    self.watcher,
                    self.sink,
                ));
            }
        }
    }

    /// Connect to the daemon, starting it (once, however many clients race) when none
    /// answers.
    fn connect_or_start(options: &ClientOptions) -> Result<UnixStream, ConnectError> {
        let socket = &options.socket;
        if socket.as_os_str().len() > crate::paths::SOCKET_PATH_LIMIT {
            return Err(ConnectError::Unavailable(format!(
                "socket path too long: {}",
                socket.display()
            )));
        }
        // Only a daemon of this user may answer.
        crate::paths::private_directory(socket)?;
        if let Ok(stream) = UnixStream::connect(socket) {
            return Ok(stream);
        }
        let Some(launch) = &options.launch else {
            return Err(ConnectError::Unavailable("no daemon answers".into()));
        };
        // Clients starting at once queue here; the first starts the daemon, the rest find
        // it answering.
        let lock = std::fs::File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(socket.with_extension("lock"))?;
        lock.lock()?;
        if let Ok(stream) = UnixStream::connect(socket) {
            return Ok(stream);
        }
        let log = std::fs::File::create(socket.with_extension("log"))
            .map_or_else(|_| std::process::Stdio::null(), std::process::Stdio::from);
        let mut child = std::process::Command::new(&launch.program)
            .args(&launch.args)
            .arg(socket)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(log)
            // Its own process group: terminal signals for the client do not reach it.
            .process_group(0)
            .spawn()?;
        // Reap it whenever it exits, so it never lingers as a zombie of this process.
        let _ = std::thread::Builder::new()
            .name("bcode-git-daemon-reaper".into())
            .spawn(move || {
                let _ = child.wait();
            });
        let deadline = Instant::now() + options.start_timeout;
        loop {
            if let Ok(stream) = UnixStream::connect(socket) {
                return Ok(stream);
            }
            if Instant::now() >= deadline {
                return Err(ConnectError::Unavailable(
                    "the started daemon did not answer".into(),
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

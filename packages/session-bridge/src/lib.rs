#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Use real Bcode sessions from an application built outside the Bcode workspace.
//!
//! [`BridgeClient::spawn_installed`] starts `bcode session-bridge --stdio` from the user's
//! installed Bcode (`BCODE_BIN`, else the first `bcode` on `PATH`) and negotiates the bridge
//! protocol version. The bridge is part of the installed Bcode, so it always matches its daemon;
//! the application only needs a protocol version inside the bridge's window, not the same Bcode
//! build. Sessions created here are ordinary Bcode sessions: `bcode tui` lists, opens and
//! continues them.
//!
//! This crate depends on serde, thiserror and tokio only, not on the Bcode runtime. Requests are
//! correlated by id, so one client can be shared by many tasks; unsolicited events (observation
//! snapshots, tool invocations) arrive on [`BridgeEvents`].
//!
//! The bridge lives as long as its stdin: [`BridgeClient::close`] (or dropping every clone)
//! ends it, which drops the application's tool registrations. Turns already running keep
//! running.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub use bcode_session_bridge_models as models;
use bcode_session_bridge_models::{
    AgentInfo, AgentProfile, BridgeError, BridgeEvent, BridgeFrame, BridgeRequest, BridgeResponse,
    BridgeResult, ClientFrame, ContextBlock, Hello, ModelChoice, ModelInfo, Origin, OriginFilter,
    PendingPermission, SessionPage, SessionSummary, SessionView, ToolDescriptor, ToolOutcome,
    Usage, VersionRange,
};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

/// Environment variable naming the Bcode executable to use instead of `bcode` on `PATH`.
pub const BCODE_BIN_ENV: &str = "BCODE_BIN";

/// Events buffered before the reader waits for the application.
const EVENT_BUFFER: usize = 256;

/// Failure of a bridge operation.
#[derive(Debug, thiserror::Error)]
pub enum BridgeClientError {
    /// No Bcode executable was found.
    #[error("Bcode is not installed: no `bcode` executable on PATH (set BCODE_BIN to choose one)")]
    NotInstalled,
    /// `BCODE_BIN` names something that is not an executable file.
    #[error("BCODE_BIN does not name an executable file: {}", .0.display())]
    InvalidOverride(PathBuf),
    /// The bridge process could not be started.
    #[error("could not start the Bcode session bridge: {0}")]
    Spawn(#[source] std::io::Error),
    /// The bridge closed its output, or the client was closed.
    #[error("the Bcode session bridge closed")]
    Closed,
    /// The bridge rejected the request; its code is stable.
    #[error(transparent)]
    Bridge(#[from] BridgeError),
    /// The bridge answered with a response of another kind.
    #[error("the Bcode session bridge sent an unexpected response")]
    UnexpectedResponse,
    /// A frame could not be encoded or written.
    #[error("could not write to the Bcode session bridge: {0}")]
    Write(#[source] std::io::Error),
}

impl BridgeClientError {
    /// The bridge's stable error code, when the bridge rejected the request.
    #[must_use]
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Bridge(error) => Some(&error.code),
            _ => None,
        }
    }
}

type Writer = Box<dyn AsyncWrite + Send + Unpin>;
type Routes = Arc<Mutex<BTreeMap<u64, oneshot::Sender<BridgeResult>>>>;

struct Shared {
    writer: tokio::sync::Mutex<Option<Writer>>,
    routes: Routes,
    next_id: AtomicU64,
    reader: tokio::task::JoinHandle<()>,
    child: tokio::sync::Mutex<Option<tokio::process::Child>>,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Shared")
            .field("next_id", &self.next_id)
            .finish_non_exhaustive()
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

/// Unsolicited events from the bridge, in arrival order. Ends when the bridge closes.
#[derive(Debug)]
pub struct BridgeEvents {
    receiver: mpsc::Receiver<BridgeEvent>,
}

impl BridgeEvents {
    /// The next event, or `None` once the bridge closed.
    pub async fn recv(&mut self) -> Option<BridgeEvent> {
        self.receiver.recv().await
    }
}

/// A connected, version-negotiated session bridge. Cloning shares the connection.
#[derive(Debug, Clone)]
pub struct BridgeClient {
    shared: Arc<Shared>,
    hello: Hello,
}

/// The installed Bcode executable: `BCODE_BIN` when set, else the first `bcode` on `PATH`.
///
/// # Errors
///
/// * [`BridgeClientError::InvalidOverride`] when `BCODE_BIN` is set but is not an executable
///   file.
/// * [`BridgeClientError::NotInstalled`] when no `bcode` is on `PATH`.
pub fn installed_bcode_executable() -> Result<PathBuf, BridgeClientError> {
    if let Some(value) = std::env::var_os(BCODE_BIN_ENV).filter(|value| !value.is_empty()) {
        let path = PathBuf::from(value);
        return if is_executable(&path) {
            Ok(path)
        } else {
            Err(BridgeClientError::InvalidOverride(path))
        };
    }
    std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .map(|directory| directory.join(if cfg!(windows) { "bcode.exe" } else { "bcode" }))
        .find(|candidate| is_executable(candidate))
        .ok_or(BridgeClientError::NotInstalled)
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

impl BridgeClient {
    /// Start the installed Bcode's session bridge and negotiate the protocol version.
    ///
    /// The bridge inherits this process's environment, so it serves the same state and
    /// configuration scope (`BCODE_STATE_DIR`, `XDG_CONFIG_HOME`). It starts the daemon when
    /// none is running.
    ///
    /// # Errors
    ///
    /// Returns [`installed_bcode_executable`]'s errors, [`BridgeClientError::Spawn`] when the
    /// process cannot start, and [`Self::connect`]'s errors.
    pub async fn spawn_installed(
        client_name: &str,
    ) -> Result<(Self, BridgeEvents), BridgeClientError> {
        Self::spawn(&installed_bcode_executable()?, client_name).await
    }

    /// Start `executable session-bridge --stdio` and negotiate the protocol version.
    ///
    /// # Errors
    ///
    /// Returns [`BridgeClientError::Spawn`] when the process cannot start, and
    /// [`Self::connect`]'s errors.
    pub async fn spawn(
        executable: &Path,
        client_name: &str,
    ) -> Result<(Self, BridgeEvents), BridgeClientError> {
        let mut child = tokio::process::Command::new(executable)
            .args(["session-bridge", "--stdio"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(BridgeClientError::Spawn)?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(BridgeClientError::Spawn(std::io::Error::other(
                "the bridge process has no stdio pipes",
            )));
        };
        let (client, events) =
            Self::connect(stdout, stdin, client_name, VersionRange::supported()).await?;
        *client.shared.child.lock().await = Some(child);
        Ok((client, events))
    }

    /// Speak the bridge protocol over `reader`/`writer` and negotiate a version in `versions`.
    ///
    /// # Errors
    ///
    /// * [`BridgeClientError::Bridge`] with code `version_mismatch` when the bridge shares no
    ///   version with `versions`; its message names the side to update.
    /// * [`BridgeClientError::Closed`] or [`BridgeClientError::Write`] when the transport fails.
    pub async fn connect<R, W>(
        reader: R,
        writer: W,
        client_name: &str,
        versions: VersionRange,
    ) -> Result<(Self, BridgeEvents), BridgeClientError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let routes = Routes::default();
        let (events, receiver) = mpsc::channel(EVENT_BUFFER);
        let reader = tokio::spawn(read_frames(reader, Arc::clone(&routes), events));
        let shared = Arc::new(Shared {
            writer: tokio::sync::Mutex::new(Some(Box::new(writer))),
            routes,
            next_id: AtomicU64::new(1),
            reader,
            child: tokio::sync::Mutex::new(None),
        });
        let response = send(
            &shared,
            BridgeRequest::Hello {
                client_name: client_name.to_owned(),
                versions,
            },
        )
        .await?;
        let BridgeResponse::Hello(hello) = response else {
            return Err(BridgeClientError::UnexpectedResponse);
        };
        Ok((Self { shared, hello }, BridgeEvents { receiver }))
    }

    /// What the bridge negotiated: the version, its window and the installed Bcode's version.
    #[must_use]
    pub const fn hello(&self) -> &Hello {
        &self.hello
    }

    /// Send any request and return its response.
    ///
    /// # Errors
    ///
    /// Returns [`BridgeClientError::Bridge`] when the bridge rejects the request, and
    /// [`BridgeClientError::Closed`] or [`BridgeClientError::Write`] when the transport fails.
    pub async fn request(
        &self,
        request: BridgeRequest,
    ) -> Result<BridgeResponse, BridgeClientError> {
        send(&self.shared, request).await
    }

    async fn done(&self, request: BridgeRequest) -> Result<(), BridgeClientError> {
        match self.request(request).await? {
            BridgeResponse::Done => Ok(()),
            _ => Err(BridgeClientError::UnexpectedResponse),
        }
    }

    async fn session(&self, request: BridgeRequest) -> Result<SessionSummary, BridgeClientError> {
        match self.request(request).await? {
            BridgeResponse::Session { session } => Ok(session),
            _ => Err(BridgeClientError::UnexpectedResponse),
        }
    }

    /// Create a session in the user's store, optionally selecting an agent and a model. A
    /// session whose origin names this application's registered provider is claimed for its
    /// tools.
    ///
    /// # Errors
    ///
    /// Returns the bridge's rejection, such as `session_origin_invalid`, or a transport error.
    pub async fn create_session(
        &self,
        name: Option<String>,
        working_directory: impl Into<String>,
        origin: Option<Origin>,
        agent: Option<String>,
        model: Option<ModelChoice>,
    ) -> Result<SessionSummary, BridgeClientError> {
        self.session(BridgeRequest::CreateSession {
            name,
            working_directory: working_directory.into(),
            origin,
            agent,
            model,
        })
        .await
    }

    /// A bounded page of sessions, newest activity first, optionally filtered by origin.
    ///
    /// # Errors
    ///
    /// Returns the bridge's rejection, such as an invalid cursor, or a transport error.
    pub async fn list_sessions(
        &self,
        origin: Option<OriginFilter>,
        working_directory: Option<String>,
        cursor: Option<String>,
        limit: u32,
    ) -> Result<SessionPage, BridgeClientError> {
        match self
            .request(BridgeRequest::ListSessions {
                text: None,
                origin,
                working_directory,
                cursor,
                limit,
            })
            .await?
        {
            BridgeResponse::Sessions(page) => Ok(page),
            _ => Err(BridgeClientError::UnexpectedResponse),
        }
    }

    /// One session's summary.
    ///
    /// # Errors
    ///
    /// Returns `session_not_found` or a transport error.
    pub async fn summary(&self, session_id: &str) -> Result<SessionSummary, BridgeClientError> {
        self.session(BridgeRequest::Summary {
            session_id: session_id.to_owned(),
        })
        .await
    }

    /// A bounded transcript page ending before `before_sequence` (the newest when `None`).
    ///
    /// # Errors
    ///
    /// Returns the bridge's rejection or a transport error.
    pub async fn history_page(
        &self,
        session_id: &str,
        before_sequence: Option<u64>,
        max_items: u32,
    ) -> Result<SessionView, BridgeClientError> {
        match self
            .request(BridgeRequest::HistoryPage {
                session_id: session_id.to_owned(),
                before_sequence,
                max_items,
            })
            .await?
        {
            BridgeResponse::History { view } => Ok(view),
            _ => Err(BridgeClientError::UnexpectedResponse),
        }
    }

    /// Observe a session; snapshots arrive as [`BridgeEvent::Observation`] with the returned id.
    /// After a [`BridgeEvent::ObservationEnded`], observe again for a fresh snapshot.
    ///
    /// # Errors
    ///
    /// Returns the bridge's rejection or a transport error.
    pub async fn observe(
        &self,
        session_id: &str,
        max_items: u32,
    ) -> Result<String, BridgeClientError> {
        match self
            .request(BridgeRequest::Observe {
                session_id: session_id.to_owned(),
                max_items,
            })
            .await?
        {
            BridgeResponse::Observing { observation_id } => Ok(observation_id),
            _ => Err(BridgeClientError::UnexpectedResponse),
        }
    }

    /// Stop an observation.
    ///
    /// # Errors
    ///
    /// Returns a transport error.
    pub async fn stop_observing(&self, observation_id: &str) -> Result<(), BridgeClientError> {
        self.done(BridgeRequest::StopObserving {
            observation_id: observation_id.to_owned(),
        })
        .await
    }

    /// Submit `text` as a new turn with optional context blocks and agent; returns the turn id
    /// and whether it waits behind another turn. Do not retry blindly after a transport error:
    /// the turn may already be admitted.
    ///
    /// # Errors
    ///
    /// Returns the bridge's rejection, such as `agent_profile_unavailable` or
    /// `invalid_turn_context`, or a transport error.
    pub async fn submit(
        &self,
        session_id: &str,
        text: impl Into<String>,
        context: Vec<ContextBlock>,
        agent: Option<String>,
    ) -> Result<(String, bool), BridgeClientError> {
        match self
            .request(BridgeRequest::Submit {
                session_id: session_id.to_owned(),
                text: text.into(),
                context,
                model: None,
                effort: None,
                agent,
            })
            .await?
        {
            BridgeResponse::Submitted { turn_id, queued } => Ok((turn_id, queued)),
            _ => Err(BridgeClientError::UnexpectedResponse),
        }
    }

    /// Cancel the session's running turn.
    ///
    /// # Errors
    ///
    /// Returns the bridge's rejection or a transport error.
    pub async fn cancel_turn(&self, session_id: &str) -> Result<(), BridgeClientError> {
        self.done(BridgeRequest::CancelTurn {
            session_id: session_id.to_owned(),
        })
        .await
    }

    /// Rename a session.
    ///
    /// # Errors
    ///
    /// Returns the bridge's rejection or a transport error.
    pub async fn rename(
        &self,
        session_id: &str,
        name: &str,
    ) -> Result<SessionSummary, BridgeClientError> {
        self.session(BridgeRequest::Rename {
            session_id: session_id.to_owned(),
            name: name.to_owned(),
        })
        .await
    }

    /// Delete a session.
    ///
    /// # Errors
    ///
    /// Returns the bridge's rejection, such as `session_connected_clients`, or a transport
    /// error.
    pub async fn delete(&self, session_id: &str) -> Result<(), BridgeClientError> {
        self.done(BridgeRequest::Delete {
            session_id: session_id.to_owned(),
        })
        .await
    }

    /// Permission requests waiting for an answer, optionally of one session.
    ///
    /// # Errors
    ///
    /// Returns a transport error.
    pub async fn pending_permissions(
        &self,
        session_id: Option<&str>,
    ) -> Result<Vec<PendingPermission>, BridgeClientError> {
        match self
            .request(BridgeRequest::PendingPermissions {
                session_id: session_id.map(str::to_owned),
            })
            .await?
        {
            BridgeResponse::Permissions { permissions } => Ok(permissions),
            _ => Err(BridgeClientError::UnexpectedResponse),
        }
    }

    /// Answer a permission request; `remember` stores the answer as a rule.
    ///
    /// # Errors
    ///
    /// Returns the bridge's rejection, such as an already answered request, or a transport
    /// error.
    pub async fn resolve_permission(
        &self,
        permission_id: &str,
        approve: bool,
        remember: bool,
    ) -> Result<(), BridgeClientError> {
        self.done(BridgeRequest::ResolvePermission {
            permission_id: permission_id.to_owned(),
            approve,
            remember,
        })
        .await
    }

    /// Register this application's tools and agent profiles under `provider_id`, replacing
    /// earlier ones. Invocations arrive as [`BridgeEvent::ToolInvoke`].
    ///
    /// # Errors
    ///
    /// Returns `client_tools_rejected` or `client_agent_profiles_rejected` when the daemon
    /// rejects the registration as a whole, or a transport error.
    pub async fn register_tools(
        &self,
        provider_id: &str,
        tools: Vec<ToolDescriptor>,
        profiles: Vec<AgentProfile>,
    ) -> Result<(), BridgeClientError> {
        match self
            .request(BridgeRequest::RegisterTools {
                provider_id: provider_id.to_owned(),
                tools,
                profiles,
            })
            .await?
        {
            BridgeResponse::ToolsRegistered { .. } => Ok(()),
            _ => Err(BridgeClientError::UnexpectedResponse),
        }
    }

    /// Offer this application's tools in a session from its next turn on.
    ///
    /// # Errors
    ///
    /// Returns `tools_not_registered`, `session_not_found` or a transport error.
    pub async fn claim_session(&self, session_id: &str) -> Result<(), BridgeClientError> {
        self.done(BridgeRequest::ClaimSession {
            session_id: session_id.to_owned(),
        })
        .await
    }

    /// Stop offering this application's tools in a session.
    ///
    /// # Errors
    ///
    /// Returns `tools_not_registered` or a transport error.
    pub async fn release_session(&self, session_id: &str) -> Result<(), BridgeClientError> {
        self.done(BridgeRequest::ReleaseSession {
            session_id: session_id.to_owned(),
        })
        .await
    }

    /// Answer a tool invocation.
    ///
    /// # Errors
    ///
    /// Returns the bridge's rejection, such as a conflicting second answer, or a transport
    /// error.
    pub async fn tool_result(
        &self,
        invocation_id: &str,
        outcome: ToolOutcome,
    ) -> Result<(), BridgeClientError> {
        self.done(BridgeRequest::ToolResult {
            invocation_id: invocation_id.to_owned(),
            outcome,
        })
        .await
    }

    /// Agents that can be selected, including profiles connected applications contribute.
    ///
    /// # Errors
    ///
    /// Returns a transport error.
    pub async fn list_agents(&self) -> Result<Vec<AgentInfo>, BridgeClientError> {
        match self.request(BridgeRequest::ListAgents).await? {
            BridgeResponse::Agents { agents } => Ok(agents),
            _ => Err(BridgeClientError::UnexpectedResponse),
        }
    }

    /// Models that can be selected.
    ///
    /// # Errors
    ///
    /// Returns a transport error.
    pub async fn list_models(&self) -> Result<Vec<ModelInfo>, BridgeClientError> {
        match self.request(BridgeRequest::ListModels).await? {
            BridgeResponse::Models { models } => Ok(models),
            _ => Err(BridgeClientError::UnexpectedResponse),
        }
    }

    /// A session's token and cost totals.
    ///
    /// # Errors
    ///
    /// Returns the bridge's rejection or a transport error.
    pub async fn usage(&self, session_id: &str) -> Result<Usage, BridgeClientError> {
        match self
            .request(BridgeRequest::Usage {
                session_id: session_id.to_owned(),
            })
            .await?
        {
            BridgeResponse::Usage(usage) => Ok(usage),
            _ => Err(BridgeClientError::UnexpectedResponse),
        }
    }

    /// Import one JSON conversation kept with the in-process SDK; returns the session and what
    /// the import could not carry over. Importing identical content again returns the same
    /// session.
    ///
    /// # Errors
    ///
    /// Returns `session_import_conflict` when different content was imported under the same
    /// conversation id, another rejection, or a transport error.
    pub async fn import_sdk_session(
        &self,
        path: impl Into<String>,
        working_directory: impl Into<String>,
        origin: Option<Origin>,
    ) -> Result<(SessionSummary, Vec<String>), BridgeClientError> {
        match self
            .request(BridgeRequest::ImportSdkSession {
                path: path.into(),
                working_directory: working_directory.into(),
                origin,
            })
            .await?
        {
            BridgeResponse::Imported { session, warnings } => Ok((session, warnings)),
            _ => Err(BridgeClientError::UnexpectedResponse),
        }
    }

    /// The command that opens a session in `bcode tui` with the installed executable.
    ///
    /// # Errors
    ///
    /// Returns a transport error.
    pub async fn open_command(&self, session_id: &str) -> Result<Vec<String>, BridgeClientError> {
        match self
            .request(BridgeRequest::OpenCommand {
                session_id: session_id.to_owned(),
            })
            .await?
        {
            BridgeResponse::Command { argv } => Ok(argv),
            _ => Err(BridgeClientError::UnexpectedResponse),
        }
    }

    /// Close the bridge's stdin, which ends the bridge and drops this application's tool
    /// registrations, and wait for a spawned bridge process to exit.
    pub async fn close(&self) {
        let writer = self.shared.writer.lock().await.take();
        if let Some(mut writer) = writer {
            let _ = writer.shutdown().await;
        }
        let child = self.shared.child.lock().await.take();
        if let Some(mut child) = child {
            let _ = child.wait().await;
        }
    }
}

async fn send(
    shared: &Shared,
    request: BridgeRequest,
) -> Result<BridgeResponse, BridgeClientError> {
    let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
    let mut line = serde_json::to_vec(&ClientFrame { id, request })
        .map_err(|error| BridgeClientError::Write(std::io::Error::other(error)))?;
    line.push(b'\n');
    let (route, response) = oneshot::channel();
    shared
        .routes
        .lock()
        .expect("bridge routes poisoned")
        .insert(id, route);
    let written = {
        let mut writer = shared.writer.lock().await;
        match writer.as_mut() {
            Some(writer) => match writer.write_all(&line).await {
                Ok(()) => writer.flush().await,
                Err(error) => Err(error),
            },
            None => Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)),
        }
    };
    if let Err(error) = written {
        shared
            .routes
            .lock()
            .expect("bridge routes poisoned")
            .remove(&id);
        return Err(if error.kind() == std::io::ErrorKind::BrokenPipe {
            BridgeClientError::Closed
        } else {
            BridgeClientError::Write(error)
        });
    }
    match response.await.map_err(|_| BridgeClientError::Closed)? {
        BridgeResult::Ok(response) => Ok(*response),
        BridgeResult::Error(error) => Err(error.into()),
    }
}

/// Route responses to their requests and forward events. Lines this version cannot decode are
/// skipped rather than ending the connection.
async fn read_frames<R>(reader: R, routes: Routes, events: mpsc::Sender<BridgeEvent>)
where
    R: AsyncRead + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        match serde_json::from_str::<BridgeFrame>(&line) {
            Ok(BridgeFrame::Response { id, result }) => {
                let route = routes.lock().expect("bridge routes poisoned").remove(&id);
                if let Some(route) = route {
                    let _ = route.send(result);
                }
            }
            // An application that dropped its event stream still gets its responses.
            Ok(BridgeFrame::Event { event }) => {
                let _ = events.send(event).await;
            }
            Err(_) => {}
        }
    }
    // Dropping the routes fails every waiting request as closed.
    routes.lock().expect("bridge routes poisoned").clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_bridge_models::{BRIDGE_PROTOCOL_VERSION, ViewItemKind};

    type Scripted = (
        Result<(BridgeClient, BridgeEvents), BridgeClientError>,
        tokio::task::JoinHandle<Vec<String>>,
    );

    /// A scripted bridge that answers each request with the next reply of `script`; `ID` in a
    /// reply is replaced by the request's id.
    async fn scripted(script: Vec<&'static str>) -> Scripted {
        let (client_side, bridge_side) = tokio::io::duplex(64 * 1024);
        let (client_read, client_write) = tokio::io::split(client_side);
        let (bridge_read, mut bridge_write) = tokio::io::split(bridge_side);
        let bridge = tokio::spawn(async move {
            let mut lines = BufReader::new(bridge_read).lines();
            let mut seen = Vec::new();
            let mut script = script.into_iter();
            while let Ok(Some(line)) = lines.next_line().await {
                let frame: ClientFrame = serde_json::from_str(&line).expect("client frame");
                seen.push(line);
                let Some(reply) = script.next() else { break };
                let reply = reply.replace("ID", &frame.id.to_string());
                for out in reply.split('\n') {
                    bridge_write.write_all(out.as_bytes()).await.expect("write");
                    bridge_write.write_all(b"\n").await.expect("write");
                }
            }
            seen
        });
        let connected =
            BridgeClient::connect(client_read, client_write, "test", VersionRange::supported())
                .await;
        (connected, bridge)
    }

    #[tokio::test]
    async fn hello_negotiates_and_responses_and_events_are_routed() {
        let hello = r#"{"frame":"response","id":ID,"ok":{"kind":"hello","version":1,"bridge_versions":{"min":1,"max":1},"bcode_version":"test"}}"#;
        let observing = "{\"frame\":\"event\",\"event\":{\"event\":\"observation\",\"observation_id\":\"o\",\"sequence\":0,\"view\":{\"session_id\":\"s\",\"items\":[{\"id\":\"1\",\"kind\":\"hologram\",\"text\":\"3D\"}]}}}\n{\"frame\":\"event\",\"event\":{\"event\":\"teleport\"}}\n{\"frame\":\"response\",\"id\":ID,\"ok\":{\"kind\":\"observing\",\"observation_id\":\"o\",\"added_later\":true}}";
        let (connected, bridge) = scripted(vec![hello, observing]).await;
        let (client, mut events) = connected.expect("connected");
        assert_eq!(client.hello().version, BRIDGE_PROTOCOL_VERSION);
        assert_eq!(client.observe("s", 10).await.expect("observe"), "o");
        let Some(BridgeEvent::Observation { view, .. }) = events.recv().await else {
            panic!("expected an observation");
        };
        assert_eq!(view.items[0].kind, ViewItemKind::Other);
        assert_eq!(view.items[0].text, "3D");
        client.close().await;
        let seen = bridge.await.expect("bridge");
        assert!(seen[0].contains(r#""op":"hello""#), "{seen:?}");
    }

    #[tokio::test]
    async fn rejections_keep_their_stable_code() {
        let mismatch = r#"{"frame":"response","id":ID,"error":{"code":"version_mismatch","message":"update Bcode"}}"#;
        let (connected, _bridge) = scripted(vec![mismatch]).await;
        let error = connected.expect_err("rejected");
        assert_eq!(error.code(), Some("version_mismatch"));
    }

    #[tokio::test]
    async fn a_closed_bridge_fails_waiting_requests() {
        let (connected, _bridge) = scripted(Vec::new()).await;
        assert!(matches!(connected, Err(BridgeClientError::Closed)));
    }
}

//! The boundary between the review UI and whatever hosts it.
//!
//! The UI reaches everything outside its own state through [`ReviewHost`]: the theme, Bcode
//! sessions, plugin services (review-source providers and publishers), the clipboard, task
//! spawning, redraw requests, and opening external targets. Repository reads and stored review
//! state are not host capabilities; the UI calls the `bcode_code_review` data layer directly.
//!
//! The standalone app uses [`StandaloneHost`]. Another host (for example the bcode chat
//! interface embedding the review) implements the same trait without changing the UI.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use bcode_code_review::provider::{ProviderCallError, ProviderTransport};
use bcode_session_models::{ProjectionWindowRequest, SessionId};
use bcode_session_view::follow::{SessionViewFollow, SessionViewFollowUpdate, follow_session_view};
use bcode_session_view_models::ReasoningPresentationPolicy;
use tokio::sync::mpsc;

use crate::theme::HostTheme;

/// Error code of host failures that have no more specific code.
pub const HOST_ERROR_FAILED: &str = "host_failed";

/// Callback that asks the run loop for a frame.
pub type Redraw = Arc<dyn Fn() + Send + Sync>;

/// Updates buffered per followed session before the follower waits for the UI.
const FOLLOW_BUFFER: usize = 16;

/// A failed host operation: a stable error code and a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostError {
    /// Stable error code (the service error code for plugin service failures).
    pub code: String,
    /// Human-readable message.
    pub message: String,
}

impl HostError {
    /// A failure with code [`HOST_ERROR_FAILED`].
    #[must_use]
    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            code: HOST_ERROR_FAILED.to_owned(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for HostError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for HostError {}

impl From<HostError> for ProviderCallError {
    fn from(error: HostError) -> Self {
        Self {
            code: error.code,
            message: error.message,
        }
    }
}

/// Owned future returned by asynchronous host operations.
pub type HostFuture<T> = Pin<Box<dyn Future<Output = Result<T, HostError>> + Send>>;

/// A detached task handed to [`ReviewHost::spawn`].
pub type HostTask = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Blocking work handed to [`ReviewHost::spawn_blocking`].
pub type BlockingTask = Box<dyn FnOnce() + Send + 'static>;

/// A program to run in the terminal, which the app hands over while it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalCommand {
    /// Program to run.
    pub program: String,
    /// Its arguments.
    pub args: Vec<String>,
}

/// Application id recorded as the origin of every session the review creates.
pub const REVIEW_SESSION_APP_ID: &str = "bcode.code-review";
/// Picker label of review sessions.
pub const REVIEW_SESSION_DISPLAY_NAME: &str = "Code review";

/// What a review session is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewSessionPurpose {
    /// An inline question about a place in the diff.
    Question,
    /// The review assistant dock's conversation.
    Assistant,
}

impl ReviewSessionPurpose {
    /// Stable origin context value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Question => "question",
            Self::Assistant => "assistant",
        }
    }
}

/// Origin context key of a review session's purpose.
pub const ORIGIN_PURPOSE: &str = "purpose";
/// Origin context key of the review workspace a session started in.
pub const ORIGIN_WORKSPACE: &str = "workspace";
/// Origin context key of the stack layer (`#<number>`) that was current when a session started.
pub const ORIGIN_LAYER: &str = "layer";

/// Origin recorded on a review session.
///
/// It names the review app, the session's purpose, the review workspace it started in (none on
/// the reviews home or in the wizard) and the stack layer that was current then. An entry is
/// left out when it would exceed the origin bounds.
#[must_use]
pub fn review_session_origin(
    purpose: ReviewSessionPurpose,
    workspace_id: Option<&str>,
    layer: Option<u64>,
) -> bcode_session_models::SessionOrigin {
    let mut origin = bcode_session_models::SessionOrigin::new(REVIEW_SESSION_APP_ID)
        .with_display_name(REVIEW_SESSION_DISPLAY_NAME)
        .with_context(ORIGIN_PURPOSE, purpose.as_str());
    let entries = [
        workspace_id
            .filter(|id| !id.is_empty())
            .map(|id| (ORIGIN_WORKSPACE, id.to_owned())),
        layer.map(|number| (ORIGIN_LAYER, format!("#{number}"))),
    ];
    for (key, value) in entries.into_iter().flatten() {
        let with = origin.clone().with_context(key, value);
        if with.validate().is_ok() {
            origin = with;
        }
    }
    origin
}

/// One block of turn context sent with a message ([`ReviewHost::submit_turn`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextBlock {
    /// Block label (1-64 bytes).
    pub label: String,
    /// Block text (never empty).
    pub text: String,
    /// Whether only this turn reads the block (it is not stored with the message).
    pub request_only: bool,
}

/// A message to a session: its text, the context blocks it carries, and the agent profile its
/// turn runs with.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TurnRequest {
    /// Exactly what the user typed.
    pub text: String,
    /// Context blocks, stored or request-only.
    pub context: Vec<ContextBlock>,
    /// Agent profile of the turn; the session's agent when `None`.
    pub agent: Option<String>,
}

/// Error code of a turn whose request-only context the daemon no longer holds.
pub const REQUEST_ONLY_CONTEXT_UNAVAILABLE: &str = "request_only_context_unavailable";

/// A Bcode session as the review app lists it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AppSession {
    /// Session id.
    pub id: String,
    /// Display title, when it has one.
    pub title: Option<String>,
    /// Creation time, in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// Last activity, in milliseconds since the Unix epoch.
    pub updated_at_ms: u64,
    /// Origin app id, when the session has an origin.
    pub origin_app: Option<String>,
    /// Origin context entries.
    pub origin_context: std::collections::BTreeMap<String, String>,
}

impl AppSession {
    /// From a Bcode session summary.
    #[must_use]
    pub fn from_summary(summary: &bcode_session_models::SessionSummary) -> Self {
        Self {
            id: summary.id.to_string(),
            title: summary.title().map(str::to_owned),
            created_at_ms: summary.created_at_ms,
            updated_at_ms: summary.updated_at_ms,
            origin_app: summary.origin.as_ref().map(|origin| origin.app_id.clone()),
            origin_context: summary
                .origin
                .as_ref()
                .map(|origin| origin.context.clone())
                .unwrap_or_default(),
        }
    }
}

/// One bounded page of the review app's sessions.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AppSessionPage {
    /// Sessions of the page, newest activity first.
    pub sessions: Vec<AppSession>,
    /// Cursor of the next page, when there is one.
    pub next_cursor: Option<String>,
    /// Whether Bcode's catalog was still loading or incomplete.
    pub degraded: bool,
}

/// A permission request waiting for an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPermission {
    /// Session that asks.
    pub session_id: String,
    /// Request id.
    pub permission_id: String,
    /// Tool that asks.
    pub tool: String,
}

/// User messages counted in a session's newest history page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TurnCount {
    /// Counted user messages.
    pub turns: usize,
    /// Whether older history was not read (the count is a lower bound).
    pub more: bool,
}

/// A call of one of the app's own tools, sent by Bcode after the call passed its permission
/// rules.
#[derive(Debug)]
pub struct ClientToolCall {
    /// Session that called.
    pub session_id: String,
    /// Tool name.
    pub tool: String,
    /// JSON arguments.
    pub arguments: String,
    /// Where the answer goes.
    pub reply: tokio::sync::oneshot::Sender<ClientToolReply>,
}

/// The app's answer to a [`ClientToolCall`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientToolReply {
    /// The tool ran: the model-visible result.
    Completed(String),
    /// The tool failed or refused.
    Failed {
        /// Stable error code.
        code: String,
        /// Model-visible message.
        message: String,
    },
}

/// Whether the app's tools and profile are registered with Bcode.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ToolLinkState {
    /// Connecting (or connecting again after Bcode went away).
    #[default]
    Connecting,
    /// Registered; sessions claimed are offered the tools.
    Ready,
    /// Registration failed, with why (retried later).
    Failed(String),
}

/// The app's connection that provides its tools and agent profile.
///
/// Sessions are created, claimed and released on it with [`ReviewHost::create_tool_session`],
/// [`ReviewHost::claim_tool_session`] and [`ReviewHost::release_tool_session`]; claimed sessions
/// are claimed again after every reconnect.
#[derive(Debug)]
pub struct ClientToolLink {
    /// Calls of the app's tools, oldest first.
    pub calls: mpsc::Receiver<ClientToolCall>,
    /// Registration state.
    pub state: Arc<Mutex<ToolLinkState>>,
}

/// A session request sent to the tool connection.
enum ToolSessionRequest {
    Create {
        title: String,
        working_directory: PathBuf,
        origin: bcode_session_models::SessionOrigin,
        reply: tokio::sync::oneshot::Sender<Result<String, HostError>>,
    },
    Claim {
        session: SessionId,
        reply: tokio::sync::oneshot::Sender<Result<(), HostError>>,
    },
    Release {
        session: SessionId,
    },
}

/// Sessions listed per catalog page at most.
pub const MAX_APP_SESSION_PAGE: usize = 200;
/// History events read to count a session's turns.
const TURN_COUNT_EVENTS: usize = 200;

/// Session view updates of a followed session. Dropping it stops following.
#[derive(Debug)]
pub struct SessionFollow {
    /// Snapshots and disconnection notices, oldest first.
    pub updates: mpsc::Receiver<SessionViewFollowUpdate>,
}

/// Capabilities the review UI needs from its host.
pub trait ReviewHost: Send + Sync {
    /// The current theme.
    fn theme(&self) -> HostTheme;

    /// Counter that changes whenever [`Self::theme`] does, so the UI can restyle without
    /// comparing themes on every frame.
    fn theme_generation(&self) -> u64;

    /// Create a Bcode session named `title` working in `working_directory` and record `origin`
    /// as the application that created it; returns its id.
    fn create_session(
        &self,
        title: String,
        working_directory: PathBuf,
        origin: bcode_session_models::SessionOrigin,
    ) -> HostFuture<String>;

    /// Send `text` to `session_id` as a user message.
    fn send_message(&self, session_id: String, text: String) -> HostFuture<()>;

    /// Submit `request` to `session_id` as a user message with its context blocks and agent
    /// profile.
    ///
    /// The default sends the text alone, for hosts without turn context.
    fn submit_turn(&self, session_id: String, request: TurnRequest) -> HostFuture<()> {
        self.send_message(session_id, request.text)
    }

    /// One bounded page (at most [`MAX_APP_SESSION_PAGE`]) of the sessions the review app
    /// created in `working_directory`, newest activity first.
    fn list_app_sessions(
        &self,
        working_directory: PathBuf,
        cursor: Option<String>,
        limit: usize,
    ) -> HostFuture<AppSessionPage> {
        let _ = (working_directory, cursor, limit);
        Box::pin(async { Err(HostError::failed("this host cannot list sessions")) })
    }

    /// The summaries of `session_ids`, in order; `None` for a session Bcode does not have.
    fn session_summaries(&self, session_ids: Vec<String>) -> HostFuture<Vec<Option<AppSession>>> {
        let _ = session_ids;
        Box::pin(async { Err(HostError::failed("this host cannot read sessions")) })
    }

    /// Give `session_id` the explicit name `name`.
    fn rename_session(&self, session_id: String, name: String) -> HostFuture<()> {
        let _ = (session_id, name);
        Box::pin(async { Err(HostError::failed("this host cannot rename sessions")) })
    }

    /// Delete `session_id` from Bcode.
    fn delete_session(&self, session_id: String) -> HostFuture<()> {
        let _ = session_id;
        Box::pin(async { Err(HostError::failed("this host cannot delete sessions")) })
    }

    /// Permission requests waiting for an answer, in every session.
    fn pending_permissions(&self) -> HostFuture<Vec<PendingPermission>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    /// Provide `tools` and agent `profiles` under the review app's provider id, until the
    /// returned link is dropped; registering again after Bcode restarts.
    ///
    /// # Errors
    ///
    /// * [`HostError`] when this host cannot provide tools.
    fn serve_client_tools(
        &self,
        tools: Vec<bcode_tool_models::client_tool::ClientToolDescriptor>,
        profiles: Vec<bcode_agent_profile::client_profile::ClientAgentProfile>,
    ) -> Result<ClientToolLink, HostError> {
        let _ = (tools, profiles);
        Err(HostError::failed("this host cannot provide tools"))
    }

    /// Create a session on the tool connection (see [`Self::serve_client_tools`]), claimed for
    /// the app's tools before it is returned, so its first turn already offers them.
    fn create_tool_session(
        &self,
        title: String,
        working_directory: PathBuf,
        origin: bcode_session_models::SessionOrigin,
    ) -> HostFuture<String> {
        let _ = (title, working_directory, origin);
        Box::pin(async { Err(HostError::failed("this host cannot provide tools")) })
    }

    /// Claim `session_id` for the app's tools; resolves once Bcode offers them from its next
    /// turn on.
    fn claim_tool_session(&self, session_id: String) -> HostFuture<()> {
        let _ = session_id;
        Box::pin(async { Err(HostError::failed("this host cannot provide tools")) })
    }

    /// Stop offering the app's tools to `session_id` from its next turn on.
    fn release_tool_session(&self, session_id: String) -> HostFuture<()> {
        let _ = session_id;
        Box::pin(async { Ok(()) })
    }

    /// User messages in the newest history page of `session_id`.
    fn count_turns(&self, session_id: String) -> HostFuture<TurnCount> {
        let _ = session_id;
        Box::pin(async { Err(HostError::failed("this host cannot read history")) })
    }

    /// Answer the pending permission request `permission_id`: allow it (`approved`) or decline
    /// it, once or remembered as a rule (`remember`).
    fn resolve_permission(
        &self,
        permission_id: String,
        approved: bool,
        remember: bool,
    ) -> HostFuture<()>;

    /// Stop the running turn of `session_id`.
    fn cancel_turn(&self, session_id: String) -> HostFuture<()>;

    /// Follow a session's live semantic view until the returned receiver is dropped.
    ///
    /// # Errors
    ///
    /// * [`HostError`] when the host cannot follow sessions.
    fn follow_session(
        &self,
        session_id: SessionId,
        projection: ProjectionWindowRequest,
        reasoning_policy: ReasoningPresentationPolicy,
    ) -> Result<SessionFollow, HostError>;

    /// The command that opens `session_id` in Bcode, run while the app hands over the terminal;
    /// `None` when this host cannot open sessions.
    fn open_session_command(&self, session_id: &str) -> Option<ExternalCommand>;

    /// Ids of enabled plugins serving `interface_id`.
    fn plugins_serving(&self, interface_id: String) -> HostFuture<Vec<String>>;

    /// Invoke `operation` of `interface_id` on one plugin and return the response payload.
    fn invoke_plugin(
        &self,
        plugin_id: String,
        interface_id: String,
        operation: String,
        payload: Vec<u8>,
    ) -> HostFuture<Vec<u8>>;

    /// Copy `text` to the system clipboard.
    ///
    /// # Errors
    ///
    /// * [`HostError`] when no clipboard is available or copying fails.
    fn copy_to_clipboard(&self, text: &str) -> Result<(), HostError>;

    /// Open a URL (such as a pull request) without suspending the UI.
    ///
    /// # Errors
    ///
    /// * [`HostError`] when the opener cannot be started.
    fn open_external(&self, url: &str) -> Result<(), HostError>;

    /// Run `task` in the background and ask for a frame when it finishes, so the UI collects
    /// its result.
    fn spawn(&self, task: HostTask);

    /// Run blocking `task` off the UI thread and ask for a frame when it finishes.
    fn spawn_blocking(&self, task: BlockingTask);

    /// Ask for a new frame; repeated requests before the frame coalesce.
    fn request_redraw(&self);

    /// Ask for a new frame once `delay` has passed (the next frame of an animation).
    fn request_redraw_after(&self, delay: Duration);

    /// Route [`Self::request_redraw`] (and the frames requested when spawned work finishes) to
    /// `redraw`, the wake-up of the loop that drives the UI. The app's own run loop connects it
    /// once it starts; a host that drives the UI from its own loop wakes that loop instead and
    /// can ignore it.
    fn connect_redraw(&self, redraw: Redraw) {
        let _ = redraw;
    }
}

/// [`ProviderTransport`] over a host's plugin services, for review-source provider routing.
pub struct HostTransport<'a>(pub &'a dyn ReviewHost);

impl ProviderTransport for HostTransport<'_> {
    fn plugins_serving(
        &self,
        interface_id: &str,
    ) -> impl Future<Output = Result<Vec<String>, ProviderCallError>> + Send {
        let future = self.0.plugins_serving(interface_id.to_owned());
        async move { future.await.map_err(ProviderCallError::from) }
    }

    fn invoke(
        &self,
        plugin_id: &str,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> impl Future<Output = Result<Vec<u8>, ProviderCallError>> + Send {
        let future = self.0.invoke_plugin(
            plugin_id.to_owned(),
            interface_id.to_owned(),
            operation.to_owned(),
            payload,
        );
        async move { future.await.map_err(ProviderCallError::from) }
    }
}

/// How the standalone host reaches Bcode.
#[derive(Debug, Clone)]
pub enum BcodeAccess {
    /// The app runs inside the Bcode executable (`bcode review`): `client` reaches its daemon
    /// (and may start it), and sessions open with `program`.
    Running {
        /// Client of the running Bcode's daemon.
        client: Box<bcode_client::BcodeClient>,
        /// The running Bcode executable.
        program: PathBuf,
    },
    /// The app is its own executable (`bcode-review`): the user's installed Bcode is located,
    /// verified, and started when needed on first use
    /// ([`bcode_client::BcodeClient::for_installed_bcode`]).
    Installed,
}

/// The Bcode daemon behind the standalone host, connected lazily for [`BcodeAccess::Installed`]
/// and connected again after the daemon goes away.
struct BcodeConnection {
    access: BcodeAccess,
    installed: tokio::sync::Mutex<Option<bcode_client::InstalledBcode>>,
}

impl BcodeConnection {
    async fn client(&self) -> Result<bcode_client::BcodeClient, HostError> {
        if let BcodeAccess::Running { client, .. } = &self.access {
            return Ok((**client).clone());
        }
        let mut installed = self.installed.lock().await;
        if let Some(installed) = installed.as_ref() {
            return Ok(installed.client().clone());
        }
        let connected = bcode_client::BcodeClient::for_installed_bcode(
            bcode_client::InstalledDaemonStart::IfNeeded,
        )
        .await
        .map_err(|error| HostError::failed(error.to_string()))?;
        let client = connected.client().clone();
        *installed = Some(connected);
        drop(installed);
        Ok(client)
    }

    /// Map a failed call, forgetting an installed connection whose daemon is gone or was
    /// replaced so the next call locates and verifies Bcode again.
    fn failure(&self, error: &bcode_client::ClientError) -> HostError {
        let gone = error.is_daemon_unavailable()
            || matches!(error, bcode_client::ClientError::IncompatibleDaemon { .. });
        if gone
            && matches!(self.access, BcodeAccess::Installed)
            && let Ok(mut installed) = self.installed.try_lock()
        {
            *installed = None;
        }
        client_error(error)
    }

    /// The Bcode executable that opens sessions.
    fn program(&self) -> String {
        let program = match &self.access {
            BcodeAccess::Running { program, .. } => Some(program.clone()),
            BcodeAccess::Installed => self
                .installed
                .try_lock()
                .ok()
                .and_then(|installed| {
                    installed
                        .as_ref()
                        .map(|installed| installed.executable().to_path_buf())
                })
                .or_else(bcode_client::installed_bcode_executable),
        };
        program.map_or_else(
            || "bcode".to_owned(),
            |program| program.to_string_lossy().into_owned(),
        )
    }
}

/// Host of the standalone app: a Bcode daemon for sessions and plugin services, the platform
/// clipboard and opener, and the app's own runtime for tasks and redraws.
pub struct StandaloneHost {
    bcode: Arc<BcodeConnection>,
    runtime: tokio::runtime::Handle,
    theme: Mutex<(HostTheme, u64)>,
    redraw: Arc<OnceLock<Redraw>>,
    /// Session requests for the tool connection, once it was asked for.
    tool_sessions: Mutex<Option<mpsc::UnboundedSender<ToolSessionRequest>>>,
}

impl std::fmt::Debug for StandaloneHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StandaloneHost")
            .field("bcode", &self.bcode.access)
            .finish_non_exhaustive()
    }
}

impl StandaloneHost {
    /// Host reaching Bcode through `access` on the current Tokio runtime, presenting `theme`.
    ///
    /// # Panics
    ///
    /// Panics when called outside a Tokio runtime.
    #[must_use]
    pub fn new(access: BcodeAccess, theme: HostTheme) -> Self {
        Self {
            bcode: Arc::new(BcodeConnection {
                access,
                installed: tokio::sync::Mutex::new(None),
            }),
            runtime: tokio::runtime::Handle::current(),
            theme: Mutex::new((theme, 0)),
            redraw: Arc::default(),
            tool_sessions: Mutex::new(None),
        }
    }

    /// Send `request` to the tool connection.
    fn tool_session_request(&self, request: ToolSessionRequest) -> Result<(), HostError> {
        self.tool_sessions
            .lock()
            .ok()
            .and_then(|sender| sender.clone())
            .ok_or_else(|| HostError::failed("the app's tools are not being served"))?
            .send(request)
            .map_err(|_| HostError::failed("the app's tool connection closed"))
    }

    /// Present `theme` from now on and request a redraw.
    ///
    /// # Panics
    ///
    /// Panics when the theme lock is poisoned.
    pub fn set_theme(&self, theme: HostTheme) {
        {
            let mut current = self.theme.lock().expect("theme lock");
            current.0 = theme;
            current.1 = current.1.wrapping_add(1);
        }
        self.request_redraw();
    }
}

fn parse_session(session_id: &str) -> Result<SessionId, HostError> {
    session_id
        .parse::<SessionId>()
        .map_err(|error| HostError::failed(error.to_string()))
}

fn client_error(error: &bcode_client::ClientError) -> HostError {
    if error.is_daemon_unavailable() {
        HostError::failed("Bcode is not running; start bcode and try again")
    } else if let bcode_client::ClientError::Server { code, message } = error {
        HostError {
            code: code.clone(),
            message: message.clone(),
        }
    } else {
        HostError::failed(error.to_string())
    }
}

impl ReviewHost for StandaloneHost {
    fn theme(&self) -> HostTheme {
        self.theme.lock().expect("theme lock").0.clone()
    }

    fn theme_generation(&self) -> u64 {
        self.theme.lock().expect("theme lock").1
    }

    fn create_session(
        &self,
        title: String,
        working_directory: PathBuf,
        origin: bcode_session_models::SessionOrigin,
    ) -> HostFuture<String> {
        let bcode = Arc::clone(&self.bcode);
        Box::pin(async move {
            bcode
                .client()
                .await?
                .create_session_with_origin(Some(title), working_directory, Some(origin))
                .await
                .map(|session| session.id.to_string())
                .map_err(|error| bcode.failure(&error))
        })
    }

    fn send_message(&self, session_id: String, text: String) -> HostFuture<()> {
        let bcode = Arc::clone(&self.bcode);
        Box::pin(async move {
            let id = session_id
                .parse::<SessionId>()
                .map_err(|error| HostError::failed(error.to_string()))?;
            bcode
                .client()
                .await?
                .send_user_message(id, text, bcode_session_models::PromptPlacement::FollowUp)
                .await
                .map(|_| ())
                .map_err(|error| bcode.failure(&error))
        })
    }

    fn submit_turn(&self, session_id: String, request: TurnRequest) -> HostFuture<()> {
        let bcode = Arc::clone(&self.bcode);
        Box::pin(async move {
            let id = parse_session(&session_id)?;
            let mut context = bcode_session_models::TurnContext::default();
            for block in request.context {
                let target = if block.request_only {
                    &mut context.request_only
                } else {
                    &mut context.blocks
                };
                target.push(bcode_session_models::TurnContextBlock::new(
                    block.label,
                    block.text,
                ));
            }
            let admission = bcode_session_models::TurnAdmissionMetadata {
                execution: bcode_session_models::TurnExecutionOptions {
                    agent_profile: request.agent,
                    ..bcode_session_models::TurnExecutionOptions::default()
                },
                context,
                ..bcode_session_models::TurnAdmissionMetadata::default()
            };
            bcode
                .client()
                .await?
                .submit_turn(id, request.text, admission)
                .await
                .map(drop)
                .map_err(|error| bcode.failure(&error))
        })
    }

    fn list_app_sessions(
        &self,
        working_directory: PathBuf,
        cursor: Option<String>,
        limit: usize,
    ) -> HostFuture<AppSessionPage> {
        let bcode = Arc::clone(&self.bcode);
        Box::pin(async move {
            let cursor = cursor
                .map(|cursor| serde_json::from_str(&cursor))
                .transpose()
                .map_err(|error| HostError::failed(format!("invalid session cursor: {error}")))?;
            let query = bcode_session_models::SessionCatalogQuery {
                text: None,
                origin: Some(bcode_session_models::SessionOriginFilter {
                    app_id: Some(REVIEW_SESSION_APP_ID.to_owned()),
                    context: std::collections::BTreeMap::new(),
                }),
            };
            let page = bcode
                .client()
                .await?
                .list_sessions_page_with_scope(
                    bcode_session_models::SessionDiscoveryScope::WorkingDirectory(
                        working_directory,
                    ),
                    query,
                    cursor,
                    limit.clamp(1, MAX_APP_SESSION_PAGE),
                )
                .await
                .map_err(|error| bcode.failure(&error))?;
            Ok(AppSessionPage {
                sessions: page.sessions.iter().map(AppSession::from_summary).collect(),
                next_cursor: page
                    .next_cursor
                    .and_then(|cursor| serde_json::to_string(&cursor).ok()),
                degraded: !matches!(
                    page.catalog_status,
                    bcode_session_models::SessionCatalogStatus::Loaded
                ),
            })
        })
    }

    fn session_summaries(&self, session_ids: Vec<String>) -> HostFuture<Vec<Option<AppSession>>> {
        let bcode = Arc::clone(&self.bcode);
        Box::pin(async move {
            let client = bcode.client().await?;
            let mut summaries = Vec::with_capacity(session_ids.len());
            for session_id in session_ids {
                let Ok(id) = session_id.parse::<SessionId>() else {
                    summaries.push(None);
                    continue;
                };
                match client.session_summary(id).await {
                    Ok(summary) => summaries.push(Some(AppSession::from_summary(&summary))),
                    Err(error) if error.is_daemon_unavailable() => {
                        return Err(bcode.failure(&error));
                    }
                    // Any other failure: Bcode does not have the session (any more).
                    Err(_) => summaries.push(None),
                }
            }
            Ok(summaries)
        })
    }

    fn rename_session(&self, session_id: String, name: String) -> HostFuture<()> {
        let bcode = Arc::clone(&self.bcode);
        Box::pin(async move {
            let id = parse_session(&session_id)?;
            bcode
                .client()
                .await?
                .rename_session(id, Some(name))
                .await
                .map(drop)
                .map_err(|error| bcode.failure(&error))
        })
    }

    fn delete_session(&self, session_id: String) -> HostFuture<()> {
        let bcode = Arc::clone(&self.bcode);
        Box::pin(async move {
            let id = parse_session(&session_id)?;
            bcode
                .client()
                .await?
                .delete_session(id)
                .await
                .map(drop)
                .map_err(|error| bcode.failure(&error))
        })
    }

    fn pending_permissions(&self) -> HostFuture<Vec<PendingPermission>> {
        let bcode = Arc::clone(&self.bcode);
        Box::pin(async move {
            let permissions = bcode
                .client()
                .await?
                .list_permissions()
                .await
                .map_err(|error| bcode.failure(&error))?;
            Ok(permissions
                .into_iter()
                .map(|permission| PendingPermission {
                    session_id: permission.session_id.to_string(),
                    permission_id: permission.permission_id,
                    tool: permission.tool_name,
                })
                .collect())
        })
    }

    fn serve_client_tools(
        &self,
        tools: Vec<bcode_tool_models::client_tool::ClientToolDescriptor>,
        profiles: Vec<bcode_agent_profile::client_profile::ClientAgentProfile>,
    ) -> Result<ClientToolLink, HostError> {
        let (call_sender, calls) = mpsc::channel(TOOL_CALL_BUFFER);
        let (sessions, session_receiver) = mpsc::unbounded_channel();
        let state = Arc::new(Mutex::new(ToolLinkState::Connecting));
        let provider = ToolProvider {
            bcode: Arc::clone(&self.bcode),
            tools,
            profiles,
            calls: call_sender,
            sessions: session_receiver,
            state: Arc::clone(&state),
            redraw: Arc::clone(&self.redraw),
        };
        if let Ok(mut current) = self.tool_sessions.lock() {
            *current = Some(sessions);
        }
        drop(self.runtime.spawn(provider.run()));
        Ok(ClientToolLink { calls, state })
    }

    fn create_tool_session(
        &self,
        title: String,
        working_directory: PathBuf,
        origin: bcode_session_models::SessionOrigin,
    ) -> HostFuture<String> {
        let (reply, answer) = tokio::sync::oneshot::channel();
        let sent = self.tool_session_request(ToolSessionRequest::Create {
            title,
            working_directory,
            origin,
            reply,
        });
        Box::pin(async move {
            sent?;
            answer
                .await
                .map_err(|_| HostError::failed("the app's tool connection closed"))?
        })
    }

    fn claim_tool_session(&self, session_id: String) -> HostFuture<()> {
        let session = parse_session(&session_id);
        let (reply, answer) = tokio::sync::oneshot::channel();
        let sent = session.and_then(|session| {
            self.tool_session_request(ToolSessionRequest::Claim { session, reply })
        });
        Box::pin(async move {
            sent?;
            answer
                .await
                .map_err(|_| HostError::failed("the app's tool connection closed"))?
        })
    }

    fn release_tool_session(&self, session_id: String) -> HostFuture<()> {
        let sent = parse_session(&session_id)
            .and_then(|session| self.tool_session_request(ToolSessionRequest::Release { session }));
        Box::pin(async move { sent })
    }

    fn count_turns(&self, session_id: String) -> HostFuture<TurnCount> {
        let bcode = Arc::clone(&self.bcode);
        Box::pin(async move {
            let id = parse_session(&session_id)?;
            let page = bcode
                .client()
                .await?
                .session_history_page(
                    id,
                    bcode_session_models::SessionHistoryQuery {
                        cursor: None,
                        limit: TURN_COUNT_EVENTS,
                        direction: bcode_session_models::SessionHistoryDirection::Backward,
                    },
                )
                .await
                .map_err(|error| bcode.failure(&error))?;
            Ok(TurnCount {
                turns: user_turns(&page.events),
                more: page.has_more,
            })
        })
    }

    fn resolve_permission(
        &self,
        permission_id: String,
        approved: bool,
        remember: bool,
    ) -> HostFuture<()> {
        let bcode = Arc::clone(&self.bcode);
        Box::pin(async move {
            let resolved = bcode
                .client()
                .await?
                .resolve_permission_with_remember(permission_id, approved, remember)
                .await
                .map_err(|error| bcode.failure(&error))?;
            if resolved {
                Ok(())
            } else {
                Err(HostError::failed(
                    "the permission request is no longer pending",
                ))
            }
        })
    }

    fn cancel_turn(&self, session_id: String) -> HostFuture<()> {
        let bcode = Arc::clone(&self.bcode);
        Box::pin(async move {
            let id = session_id
                .parse::<SessionId>()
                .map_err(|error| HostError::failed(error.to_string()))?;
            bcode
                .client()
                .await?
                .cancel_session_turn(id)
                .await
                .map(drop)
                .map_err(|error| bcode.failure(&error))
        })
    }

    fn follow_session(
        &self,
        session_id: SessionId,
        projection: ProjectionWindowRequest,
        reasoning_policy: ReasoningPresentationPolicy,
    ) -> Result<SessionFollow, HostError> {
        let (mut sender, updates) = mpsc::channel(FOLLOW_BUFFER);
        let follow = SessionViewFollow {
            session_id,
            projection,
            reasoning_policy,
            client_name: "bcode-review-session-view".to_owned(),
            interaction_kind: None,
        };
        let bcode = Arc::clone(&self.bcode);
        let redraw = Arc::clone(&self.redraw);
        drop(self.runtime.spawn(async move {
            // Without Bcode the follower ends at once, which the view reports as a closed
            // follow, like a daemon that goes away while following.
            let Ok(client) = bcode.client().await else {
                return;
            };
            let mut sink = RedrawingSink {
                sender: &mut sender,
                redraw,
            };
            Box::pin(follow_session_view(client, follow, &mut sink)).await;
        }));
        Ok(SessionFollow { updates })
    }

    fn open_session_command(&self, session_id: &str) -> Option<ExternalCommand> {
        Some(ExternalCommand {
            program: self.bcode.program(),
            args: vec!["tui".to_owned(), session_id.to_owned()],
        })
    }

    fn plugins_serving(&self, interface_id: String) -> HostFuture<Vec<String>> {
        let bcode = Arc::clone(&self.bcode);
        Box::pin(async move {
            let services = bcode
                .client()
                .await?
                .plugin_services()
                .await
                .map_err(|error| bcode.failure(&error))?;
            Ok(services
                .into_iter()
                .filter(|service| service.interface_id == interface_id)
                .map(|service| service.plugin_id)
                .collect())
        })
    }

    fn invoke_plugin(
        &self,
        plugin_id: String,
        interface_id: String,
        operation: String,
        payload: Vec<u8>,
    ) -> HostFuture<Vec<u8>> {
        let bcode = Arc::clone(&self.bcode);
        Box::pin(async move {
            let response = bcode
                .client()
                .await?
                .invoke_plugin_service(plugin_id, interface_id, operation, payload)
                .await
                .map_err(|error| bcode.failure(&error))?;
            match response.error {
                Some(error) => Err(HostError {
                    code: error.code,
                    message: error.message,
                }),
                None => Ok(response.payload),
            }
        })
    }

    fn copy_to_clipboard(&self, text: &str) -> Result<(), HostError> {
        copy_to_clipboard(text).map_err(HostError::failed)
    }

    fn open_external(&self, url: &str) -> Result<(), HostError> {
        let mut command = if cfg!(target_os = "macos") {
            std::process::Command::new("open")
        } else if cfg!(windows) {
            let mut command = std::process::Command::new("cmd");
            command.args(["/C", "start", ""]);
            command
        } else {
            std::process::Command::new("xdg-open")
        };
        command
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(drop)
            .map_err(|error| HostError::failed(error.to_string()))
    }

    fn spawn(&self, task: HostTask) {
        let redraw = Arc::clone(&self.redraw);
        drop(self.runtime.spawn(async move {
            task.await;
            if let Some(redraw) = redraw.get() {
                redraw();
            }
        }));
    }

    fn spawn_blocking(&self, task: BlockingTask) {
        let redraw = Arc::clone(&self.redraw);
        drop(self.runtime.spawn_blocking(move || {
            task();
            if let Some(redraw) = redraw.get() {
                redraw();
            }
        }));
    }

    fn request_redraw(&self) {
        if let Some(redraw) = self.redraw.get() {
            redraw();
        }
    }

    fn request_redraw_after(&self, delay: Duration) {
        let redraw = Arc::clone(&self.redraw);
        drop(self.runtime.spawn(async move {
            tokio::time::sleep(delay).await;
            if let Some(redraw) = redraw.get() {
                redraw();
            }
        }));
    }

    /// Only the first call takes effect.
    fn connect_redraw(&self, redraw: Redraw) {
        let _ = self.redraw.set(redraw);
    }
}

/// Calls of the app's tools buffered before the provider waits for the UI.
const TOOL_CALL_BUFFER: usize = 32;
/// Sessions the tool connection claims again after a reconnect at most (the newest).
const MAX_RECLAIMED: usize = 32;
/// Pause before connecting the tool provider again.
const TOOL_RECONNECT_DELAY: Duration = Duration::from_secs(2);

/// The task that keeps the app's tools registered and forwards their calls to the UI.
struct ToolProvider {
    bcode: Arc<BcodeConnection>,
    tools: Vec<bcode_tool_models::client_tool::ClientToolDescriptor>,
    profiles: Vec<bcode_agent_profile::client_profile::ClientAgentProfile>,
    calls: mpsc::Sender<ClientToolCall>,
    sessions: mpsc::UnboundedReceiver<ToolSessionRequest>,
    state: Arc<Mutex<ToolLinkState>>,
    redraw: Arc<OnceLock<Redraw>>,
}

impl ToolProvider {
    fn set_state(&self, state: ToolLinkState) {
        if let Ok(mut current) = self.state.lock() {
            *current = state;
        }
        if let Some(redraw) = self.redraw.get() {
            redraw();
        }
    }

    /// Connect, register, serve until the connection closes, and connect again, until the
    /// app drops its link.
    async fn run(mut self) {
        let mut claimed: Vec<SessionId> = Vec::new();
        loop {
            match self.serve(&mut claimed).await {
                Ok(()) => return,
                Err(error) => self.set_state(ToolLinkState::Failed(error)),
            }
            if self.calls.is_closed() {
                return;
            }
            tokio::time::sleep(TOOL_RECONNECT_DELAY).await;
        }
    }

    /// One connection; `Ok` when the app dropped its link.
    async fn serve(&mut self, claimed: &mut Vec<SessionId>) -> Result<(), String> {
        use bcode_client::client_tools::ClientToolRequest;
        use bcode_tool_models::client_tool::ClientToolOutcome;
        let client = self.bcode.client().await.map_err(|error| error.message)?;
        let mut provider = client
            .client_tool_provider(REVIEW_SESSION_APP_ID, self.tools.clone())
            .await
            .map_err(|error| client_error(&error).message)?;
        provider
            .register_agent_profiles(self.profiles.clone())
            .await
            .map_err(|error| client_error(&error).message)?;
        let (handle, mut requests) = provider.into_concurrent();
        for session in claimed.iter() {
            let _ = handle.claim_session(*session).await;
        }
        self.set_state(ToolLinkState::Ready);
        loop {
            tokio::select! {
                request = requests.recv() => {
                    let Some(request) = request else {
                        return Err("Bcode closed the tool connection".to_owned());
                    };
                    let ClientToolRequest::Invoke(invocation) = request else {
                        continue;
                    };
                    let (reply, answer) = tokio::sync::oneshot::channel();
                    let call = ClientToolCall {
                        session_id: invocation.session_id.clone(),
                        tool: invocation.tool_name.clone(),
                        arguments: invocation.arguments_json.clone(),
                        reply,
                    };
                    if self.calls.send(call).await.is_err() {
                        return Ok(());
                    }
                    if let Some(redraw) = self.redraw.get() {
                        redraw();
                    }
                    let handle = handle.clone();
                    let invocation_id = invocation.invocation_id;
                    tokio::spawn(async move {
                        let outcome = match answer.await {
                            Ok(ClientToolReply::Completed(content)) => {
                                ClientToolOutcome::Completed { content }
                            }
                            Ok(ClientToolReply::Failed { code, message }) => {
                                ClientToolOutcome::Failed { code, message }
                            }
                            Err(_) => ClientToolOutcome::Failed {
                                code: "client_tool_unavailable".to_owned(),
                                message: "the review app closed".to_owned(),
                            },
                        };
                        let _ = handle.respond(invocation_id, outcome).await;
                    });
                }
                request = self.sessions.recv() => {
                    let Some(request) = request else {
                        return Ok(());
                    };
                    Self::session_request(&handle, claimed, request).await;
                }
            }
        }
    }
}

impl ToolProvider {
    /// Create, claim or release a session on the connection, keeping `claimed` (claimed again
    /// after a reconnect) in step.
    async fn session_request(
        handle: &bcode_client::client_tools::ClientToolHandle,
        claimed: &mut Vec<SessionId>,
        request: ToolSessionRequest,
    ) {
        match request {
            ToolSessionRequest::Create {
                title,
                working_directory,
                origin,
                reply,
            } => {
                let result = match handle
                    .create_session(Some(title), working_directory, Some(origin))
                    .await
                {
                    Ok(session) => {
                        // Bcode claims a session created with the provider's origin; claiming
                        // again makes sure, before the first turn is submitted.
                        let claim = handle.claim_session(session.id).await;
                        remember_claim(claimed, session.id);
                        claim
                            .map(|()| session.id.to_string())
                            .map_err(|error| client_error(&error))
                    }
                    Err(error) => Err(client_error(&error)),
                };
                let _ = reply.send(result);
            }
            ToolSessionRequest::Claim { session, reply } => {
                let result = handle
                    .claim_session(session)
                    .await
                    .map_err(|error| client_error(&error));
                if result.is_ok() {
                    remember_claim(claimed, session);
                }
                let _ = reply.send(result);
            }
            ToolSessionRequest::Release { session } => {
                claimed.retain(|known| *known != session);
                let _ = handle.release_session(session).await;
            }
        }
    }
}

/// Remember `session` as claimed (newest last), keeping at most [`MAX_RECLAIMED`].
fn remember_claim(claimed: &mut Vec<SessionId>, session: SessionId) {
    claimed.retain(|known| *known != session);
    claimed.push(session);
    if claimed.len() > MAX_RECLAIMED {
        claimed.drain(..claimed.len() - MAX_RECLAIMED);
    }
}

/// Forwards follow updates to the UI and requests a frame for each.
struct RedrawingSink<'a> {
    sender: &'a mut mpsc::Sender<SessionViewFollowUpdate>,
    redraw: Arc<OnceLock<Redraw>>,
}

impl bcode_session_view::follow::SessionViewSink for RedrawingSink<'_> {
    async fn deliver(&mut self, update: SessionViewFollowUpdate) -> bool {
        if self.sender.send(update).await.is_err() {
            return false;
        }
        if let Some(redraw) = self.redraw.get() {
            redraw();
        }
        true
    }

    fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }
}

/// Copy `text` with the platform clipboard command.
fn copy_to_clipboard(text: &str) -> Result<(), String> {
    use std::io::Write as _;
    let candidates: &[&[&str]] = if cfg!(target_os = "macos") {
        &[&["pbcopy"]]
    } else if cfg!(windows) {
        &[&["clip"]]
    } else {
        &[
            &["wl-copy"],
            &["xclip", "-selection", "clipboard"],
            &["xsel", "--clipboard", "--input"],
        ]
    };
    for candidate in candidates {
        let Ok(mut child) = std::process::Command::new(candidate[0])
            .args(&candidate[1..])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(text.as_bytes())
                .map_err(|error| error.to_string())?;
        }
        return child
            .wait()
            .map_err(|error| error.to_string())
            .and_then(|status| {
                if status.success() {
                    Ok(())
                } else {
                    Err(format!("{} failed", candidate[0]))
                }
            });
    }
    Err("no clipboard command found (pbcopy, wl-copy, xclip or xsel)".into())
}

/// The user turns among `events`: each activated submission (how a user message enters a
/// session's history), or a user message recorded directly by older sessions.
fn user_turns(events: &[bcode_session_models::SessionEvent]) -> usize {
    events
        .iter()
        .filter(|event| {
            matches!(
                event.kind,
                bcode_session_models::SessionEventKind::SubmissionActivated { .. }
                    | bcode_session_models::SessionEventKind::UserMessage { .. }
            )
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turns_count_activated_submissions_and_older_user_messages() {
        let session_id = SessionId::new();
        let client_id = bcode_session_models::ClientId::new();
        let event = |sequence, kind| bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: 1,
            session_id,
            provenance: None,
            kind,
        };
        let submission = |id: &str| bcode_session_models::PromptSubmission {
            id: bcode_session_models::SubmissionId(id.to_owned()),
            client_id,
            text: "question".to_owned(),
            admission: bcode_session_models::TurnAdmissionMetadata::default(),
            requested_placement: bcode_session_models::SubmissionPlacement::FollowUp,
            target: bcode_session_models::SubmissionTarget::OwnTurn {
                turn_id: bcode_session_models::TurnId(format!("turn-{id}")),
            },
            execution_descriptor: None,
        };
        let activated = |id: &str| bcode_session_models::SessionEventKind::SubmissionActivated {
            submission_id: bcode_session_models::SubmissionId(id.to_owned()),
            turn_id: bcode_session_models::TurnId(format!("turn-{id}")),
            client_id,
            text: "question".to_owned(),
            admission: bcode_session_models::TurnAdmissionMetadata::default(),
        };
        let events = [
            event(
                1,
                bcode_session_models::SessionEventKind::UserMessage {
                    client_id,
                    text: "older".to_owned(),
                    admission: bcode_session_models::TurnAdmissionMetadata::default(),
                },
            ),
            event(
                2,
                bcode_session_models::SessionEventKind::SubmissionAccepted {
                    submission: submission("a"),
                },
            ),
            event(3, activated("a")),
            event(
                4,
                bcode_session_models::SessionEventKind::AssistantMessage {
                    text: "answer".to_owned(),
                },
            ),
            event(
                5,
                bcode_session_models::SessionEventKind::SubmissionAccepted {
                    submission: submission("b"),
                },
            ),
            event(6, activated("b")),
        ];
        assert_eq!(user_turns(&events), 3);
    }

    #[test]
    fn origins_record_the_review_and_layer_or_neither() {
        let review =
            review_session_origin(ReviewSessionPurpose::Assistant, Some("rv-1"), Some(103));
        assert_eq!(
            review.context.get(ORIGIN_PURPOSE).map(String::as_str),
            Some("assistant")
        );
        assert_eq!(
            review.context.get(ORIGIN_WORKSPACE).map(String::as_str),
            Some("rv-1")
        );
        assert_eq!(
            review.context.get(ORIGIN_LAYER).map(String::as_str),
            Some("#103")
        );
        let home = review_session_origin(ReviewSessionPurpose::Assistant, None, None);
        assert!(!home.context.contains_key(ORIGIN_WORKSPACE));
        assert!(!home.context.contains_key(ORIGIN_LAYER));
        // An id beyond the origin bounds is left out rather than failing the session.
        let long = "x".repeat(1_000);
        let bounded = review_session_origin(ReviewSessionPurpose::Question, Some(&long), None);
        assert!(bounded.validate().is_ok());
        assert!(!bounded.context.contains_key(ORIGIN_WORKSPACE));
    }
}

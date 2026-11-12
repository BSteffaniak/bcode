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

use bcode_code_review::provider::{ProviderCallError, ProviderTransport};
use bcode_session_models::{ProjectionWindowRequest, SessionId};
use bcode_session_view::follow::{SessionViewFollow, SessionViewFollowUpdate, follow_session_view};
use bcode_session_view_models::ReasoningPresentationPolicy;
use tokio::sync::mpsc;

use crate::theme::HostTheme;

/// Error code of host failures that have no more specific code.
pub const HOST_ERROR_FAILED: &str = "host_failed";

/// Callback that asks the run loop for a frame.
type Redraw = Arc<dyn Fn() + Send + Sync>;

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

    /// Create a Bcode session named `title` working in `working_directory`; returns its id.
    fn create_session(&self, title: String, working_directory: PathBuf) -> HostFuture<String>;

    /// Send `text` to `session_id` as a user message.
    fn send_message(&self, session_id: String, text: String) -> HostFuture<()>;

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

/// Host of the standalone app: the local Bcode daemon for sessions and plugin services, the
/// platform clipboard and opener, and the app's own runtime for tasks and redraws.
pub struct StandaloneHost {
    client: bcode_client::BcodeClient,
    runtime: tokio::runtime::Handle,
    theme: Mutex<(HostTheme, u64)>,
    redraw: Arc<OnceLock<Redraw>>,
    bcode_program: String,
}

impl std::fmt::Debug for StandaloneHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StandaloneHost")
            .field("bcode_program", &self.bcode_program)
            .finish_non_exhaustive()
    }
}

impl StandaloneHost {
    /// Host over `client` and the current Tokio runtime, presenting `theme`.
    ///
    /// # Panics
    ///
    /// Panics when called outside a Tokio runtime.
    #[must_use]
    pub fn new(client: bcode_client::BcodeClient, theme: HostTheme) -> Self {
        Self {
            client,
            runtime: tokio::runtime::Handle::current(),
            theme: Mutex::new((theme, 0)),
            redraw: Arc::default(),
            bcode_program: "bcode".to_owned(),
        }
    }

    /// Use `program` (instead of `bcode` on the path) to open sessions.
    #[must_use]
    pub fn with_bcode_program(mut self, program: impl Into<String>) -> Self {
        self.bcode_program = program.into();
        self
    }

    /// Route [`ReviewHost::request_redraw`] to `redraw`. Only the first call takes effect; the
    /// run loop connects its runtime here once it starts.
    pub fn connect_redraw(&self, redraw: Redraw) {
        let _ = self.redraw.set(redraw);
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

fn client_error(error: &bcode_client::ClientError) -> HostError {
    if error.is_daemon_unavailable() {
        HostError::failed("Bcode is not running; start bcode and try again")
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

    fn create_session(&self, title: String, working_directory: PathBuf) -> HostFuture<String> {
        let client = self.client.clone();
        Box::pin(async move {
            client
                .create_session_in_working_directory(Some(title), working_directory)
                .await
                .map(|session| session.id.to_string())
                .map_err(|error| client_error(&error))
        })
    }

    fn send_message(&self, session_id: String, text: String) -> HostFuture<()> {
        let client = self.client.clone();
        Box::pin(async move {
            let id = session_id
                .parse::<SessionId>()
                .map_err(|error| HostError::failed(error.to_string()))?;
            client
                .send_user_message(id, text, bcode_session_models::PromptPlacement::FollowUp)
                .await
                .map(|_| ())
                .map_err(|error| client_error(&error))
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
        let client = self.client.clone();
        let redraw = Arc::clone(&self.redraw);
        drop(self.runtime.spawn(async move {
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
            program: self.bcode_program.clone(),
            args: vec!["tui".to_owned(), session_id.to_owned()],
        })
    }

    fn plugins_serving(&self, interface_id: String) -> HostFuture<Vec<String>> {
        let client = self.client.clone();
        Box::pin(async move {
            ProviderTransport::plugins_serving(&client, &interface_id)
                .await
                .map_err(provider_error)
        })
    }

    fn invoke_plugin(
        &self,
        plugin_id: String,
        interface_id: String,
        operation: String,
        payload: Vec<u8>,
    ) -> HostFuture<Vec<u8>> {
        let client = self.client.clone();
        Box::pin(async move {
            ProviderTransport::invoke(&client, &plugin_id, &interface_id, &operation, payload)
                .await
                .map_err(provider_error)
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
}

fn provider_error(error: ProviderCallError) -> HostError {
    HostError {
        code: error.code,
        message: error.message,
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

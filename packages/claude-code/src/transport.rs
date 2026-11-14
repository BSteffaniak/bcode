use crate::{
    CallbackRequest, CallbackResponse, ControlRequest, Error, Event, LaunchOptions, NativeMessage,
    Settlement, protocol::Lifecycle,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    sync::{Notify, mpsc, oneshot, watch},
    task::JoinHandle,
};

type Pending = oneshot::Sender<Result<Value, Error>>;

/// Native events awaiting the single consumer, bounded by the bytes they hold rather than
/// by count: a healthy session streams thousands of small frames in bursts, and only a
/// consumer that has fallen this far behind is a real capacity fault.
#[derive(Clone)]
struct EventSender {
    tx: mpsc::UnboundedSender<(Event, usize)>,
    queued: Arc<AtomicUsize>,
    limit: usize,
}
/// Receiving half of [`EventSender`].
struct EventReceiver {
    rx: mpsc::UnboundedReceiver<(Event, usize)>,
    queued: Arc<AtomicUsize>,
}
/// An event queue holding at most `limit` bytes of native frames.
fn event_channel(limit: usize) -> (EventSender, EventReceiver) {
    let (tx, rx) = mpsc::unbounded_channel();
    let queued = Arc::new(AtomicUsize::new(0));
    (
        EventSender {
            tx,
            queued: Arc::clone(&queued),
            limit,
        },
        EventReceiver { rx, queued },
    )
}
impl EventSender {
    fn send(&self, event: Event, bytes: usize) -> Result<(), Error> {
        let bytes = bytes.max(1);
        let held = self.queued.fetch_add(bytes, Ordering::AcqRel);
        if held.saturating_add(bytes) > self.limit {
            self.queued.fetch_sub(bytes, Ordering::AcqRel);
            return Err(Error::Capacity);
        }
        self.tx.send((event, bytes)).map_err(|_| {
            self.queued.fetch_sub(bytes, Ordering::AcqRel);
            Error::Closed
        })
    }
}
impl EventReceiver {
    async fn recv(&mut self) -> Option<Event> {
        let (event, bytes) = self.rx.recv().await?;
        self.queued.fetch_sub(bytes, Ordering::AcqRel);
        Some(event)
    }
    #[cfg(test)]
    fn try_recv(&mut self) -> Result<Event, mpsc::error::TryRecvError> {
        let (event, bytes) = self.rx.try_recv()?;
        self.queued.fetch_sub(bytes, Ordering::AcqRel);
        Ok(event)
    }
}
struct State {
    pending: BTreeMap<String, Pending>,
    callbacks: BTreeMap<String, CallbackRequest>,
    responding: BTreeSet<String>,
    lifecycle: Lifecycle,
    attribution: crate::attribution::AttributionLedger,
    attributed_mode: Option<bool>,
    fault: Option<Fault>,
    expected_session_id: Option<String>,
    confirmed_session_id: Option<String>,
    initialization_request_id: Option<String>,
    /// Tools of frames skipped for uncorrelatable attribution; their task frames are skipped too.
    skipped_tools: BTreeSet<String>,
}
impl State {
    fn settlement(&self) -> Settlement {
        let snapshot = self.lifecycle.snapshot();
        if !self.callbacks.is_empty() && snapshot != Settlement::Uncertain {
            Settlement::RequiresAction
        } else {
            snapshot
        }
    }
}
#[derive(Clone, Copy)]
enum Fault {
    Closed,
    Protocol,
    Capacity,
    SessionIdentity,
}
impl Fault {
    const fn error(self) -> Error {
        match self {
            Self::Closed => Error::Closed,
            Self::Protocol => Error::Protocol,
            Self::Capacity => Error::Capacity,
            Self::SessionIdentity => Error::SessionIdentity,
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReapState {
    Pending,
    Reaped,
    Failed,
}

/// Supervisor-observed successful OS process wait, not a protocol acknowledgement.
/// This proves transport release only; it does not establish absence of tool effects.
#[derive(Debug)]
pub struct PhysicalReap {
    _private: (),
}

struct Shared {
    model_selection_contract: crate::models::ModelSelectionContract,
    /// Probed native `--version` output of this exact process launch.
    native_version: String,
    reaped: watch::Sender<ReapState>,
    state: Mutex<State>,
    stop: watch::Sender<bool>,
    writes: mpsc::Sender<Write>,
    counter: AtomicU64,
    initialized: AtomicU8,
    identity_changed: Notify,
    limits: crate::Limits,
    sdk_servers: BTreeSet<String>,
    sdk_metadata: [AtomicU64; 3],
}
impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn fail(&self, fault: Fault) {
        let mut state = self.lock();
        if state.fault.is_none() {
            state.fault = Some(fault);
        }
        state.lifecycle.disconnect();
        state.callbacks.clear();
        state.responding.clear();
        state.pending.clear();
        drop(state);
        let _ = self.stop.send(true);
        self.identity_changed.notify_waiters();
    }
    fn enqueue(
        &self,
        value: &Value,
        callback: Option<String>,
    ) -> Result<oneshot::Receiver<Result<(), Error>>, Error> {
        let mut output = BoundedFrame {
            bytes: Vec::new(),
            limit: self.limits.frame_bytes,
        };
        serde_json::to_writer(&mut output, value).map_err(|_| Error::Capacity)?;
        let mut frame = output.bytes;
        frame.push(b'\n');
        let (tx, rx) = oneshot::channel();
        self.writes
            .try_send(Write {
                frame,
                ack: tx,
                callback,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => Error::Capacity,
                mpsc::error::TrySendError::Closed(_) => Error::Closed,
            })?;
        Ok(rx)
    }
}
struct Write {
    frame: Vec<u8>,
    ack: oneshot::Sender<Result<(), Error>>,
    callback: Option<String>,
}
struct BoundedFrame {
    bytes: Vec<u8>,
    limit: usize,
}
impl std::io::Write for BoundedFrame {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        if buffer.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::OutOfMemory,
                "frame limit exceeded",
            ));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
/// Native identity evidence, independent of control-channel readiness.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdentityState {
    /// Native identity was not selected and has not been observed.
    Unselected,
    /// Launch selection only, not native acceptance or transcript existence.
    Prepared { requested_session_id: String },
    /// A native system init frame (or explicit initialization identity) matched selection.
    Confirmed { native_session_id: String },
    /// Process/protocol loss or mismatch; prior observations do not establish live association.
    Uncertain,
}
/// Native-confirmed process identity after control initialization. This is not
/// proof of a durable transcript, assignment completion, or successful resumption of work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitializedSession {
    /// Identity echoed by the native control response or top-level system init frame.
    pub native_session_id: String,
}
/// Acknowledges only that a JSONL frame was written, never native acceptance or no effects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeliveryReceipt {
    pub bytes_written: bool,
}
/// Secret-free native connection state for the registered host-tools server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostToolsConnection {
    /// Native reports a connected server; does not prove catalog visibility.
    Connected,
    /// Native reports failed connection.
    Failed,
    /// Native is still connecting.
    Pending,
    /// Native reports a disabled server.
    Disabled,
    /// Native reports authentication is required.
    NeedsAuth,
    /// No matching server was present in a valid bounded status response.
    Missing,
    /// Unsupported native status or response shape.
    Unknown,
}
impl HostToolsConnection {
    /// Stable sanitized diagnostic label; never contains native errors or configuration.
    #[must_use]
    const fn label(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Failed => "failed",
            Self::Pending => "pending",
            Self::Disabled => "disabled",
            Self::NeedsAuth => "needs_auth",
            Self::Missing => "missing",
            Self::Unknown => "unknown",
        }
    }
}
fn host_tools_connection(value: &Value) -> HostToolsConnection {
    let Some(servers) = value.get("mcpServers").and_then(Value::as_array) else {
        return HostToolsConnection::Unknown;
    };
    if servers.len() > 256 {
        return HostToolsConnection::Unknown;
    }
    let mut selected = servers
        .iter()
        .filter(|server| server["name"] == "bcode-tools");
    let Some(server) = selected.next() else {
        return HostToolsConnection::Missing;
    };
    if selected.next().is_some() {
        return HostToolsConnection::Unknown;
    }
    match server["status"].as_str() {
        Some("connected") => HostToolsConnection::Connected,
        Some("failed") => HostToolsConnection::Failed,
        Some("pending") => HostToolsConnection::Pending,
        Some("disabled") => HostToolsConnection::Disabled,
        Some("needs-auth") => HostToolsConnection::NeedsAuth,
        _ => HostToolsConnection::Unknown,
    }
}

/// Cloneable control handle. All clones address only the originally spawned process.
#[derive(Clone)]
pub struct ClaudeClient {
    shared: Arc<Shared>,
}
/// Single-consumer observation stream and process lifetime owner.
/// Dropping kills/reaps the process asynchronously and invalidates callbacks.
pub struct ClaudeSession {
    client: ClaudeClient,
    events: EventReceiver,
    supervisor: Option<JoinHandle<()>>,
}
/// Independently awaitable request. Dropping/cancelling abandons the local waiter only;
/// the native mutation might already have executed and must never be automatically replayed.
pub struct ControlTicket {
    id: String,
    response: Option<oneshot::Receiver<Result<Value, Error>>>,
    shared: Arc<Shared>,
    initialization: bool,
}
impl ControlTicket {
    /// Correlation identity is scoped to this process, not a durable submission ID.
    #[must_use]
    pub fn request_id(&self) -> &str {
        &self.id
    }
    /// Wait for a correlated response. Timeout/disconnect are uncertain, not rejection.
    /// # Errors
    /// Returns `Uncertain` on timeout/lost connection, or a sanitized native protocol error.
    pub async fn wait(mut self) -> Result<Value, Error> {
        let receiver = self.response.take().ok_or(Error::Uncertain)?;
        let result = match tokio::time::timeout(self.shared.limits.request_timeout, receiver).await
        {
            Ok(Ok(result)) => result,
            _ => Err(Error::Uncertain),
        };
        if self.initialization {
            self.shared
                .initialized
                .store(if result.is_ok() { 2 } else { 3 }, Ordering::Release);
        }
        result
    }
    /// Abandon the local waiter. This does not cancel native execution.
    pub fn cancel(self) {
        drop(self);
    }
}
impl Drop for ControlTicket {
    fn drop(&mut self) {
        if self.initialization && self.shared.initialized.load(Ordering::Acquire) == 1 {
            self.shared.initialized.store(3, Ordering::Release);
        }
        self.shared.lock().pending.remove(&self.id);
    }
}
impl ClaudeSession {
    /// Verify the explicit CLI version, then start continuously drained bounded IO.
    /// # Errors
    /// Invalid selection, version mismatch, process creation failure or invalid limits.
    pub async fn spawn(options: LaunchOptions) -> Result<Self, Error> {
        Self::spawn_selected(options, Vec::new(), None, false).await
    }
    /// Create a new native session with an explicit UUID using `--session-id`.
    /// The requested UUID is selection only: call `initialize`, submit the actual task,
    /// and observe confirmation before publishing a native-confirmed binding. This never uses `--resume` or most-recent selection.
    /// # Errors
    /// Invalid UUID, simultaneous resume selection, or the errors from `spawn`.
    pub async fn spawn_new(options: LaunchOptions, native_session_id: &str) -> Result<Self, Error> {
        crate::validate_session_uuid(native_session_id)?;
        if options.resume_session_id.is_some() {
            return Err(Error::InvalidConfiguration(
                "create and resume are mutually exclusive",
            ));
        }
        Self::spawn_selected(
            options,
            Vec::new(),
            Some(native_session_id.to_owned()),
            false,
        )
        .await
    }
    /// Launch with bounded in-process SDK MCP server names. `McpMessage` callbacks
    /// are routed to the caller; this library never executes tools itself.
    /// # Errors
    /// Same as `spawn`, or invalid/duplicate server identities.
    pub async fn spawn_with_sdk_servers(
        options: LaunchOptions,
        servers: Vec<String>,
    ) -> Result<Self, Error> {
        Self::spawn_selected(options, servers, None, false).await
    }
    /// Explicit new-session identity together with SDK MCP registrations.
    /// # Errors
    /// Same as `spawn_new` and `spawn_with_sdk_servers`.
    pub async fn spawn_new_with_sdk_servers(
        options: LaunchOptions,
        native_session_id: &str,
        servers: Vec<String>,
    ) -> Result<Self, Error> {
        crate::validate_session_uuid(native_session_id)?;
        if options.resume_session_id.is_some() {
            return Err(Error::InvalidConfiguration(
                "create and resume are mutually exclusive",
            ));
        }
        Self::spawn_selected(options, servers, Some(native_session_id.to_owned()), false).await
    }
    /// Create a fresh session with native tools and MCP disabled at launch.
    /// This is not a general no-effects sandbox or a settlement capability claim:
    /// managed hooks/configuration still require independent trust and observation.
    /// # Errors
    /// Same as `spawn_new`; resume and SDK server registration are not allowed.
    pub async fn spawn_new_without_tools(
        options: LaunchOptions,
        native_session_id: &str,
    ) -> Result<Self, Error> {
        crate::validate_session_uuid(native_session_id)?;
        if options.resume_session_id.is_some() {
            return Err(Error::InvalidConfiguration(
                "create and resume are mutually exclusive",
            ));
        }
        Self::spawn_selected(
            options,
            Vec::new(),
            Some(native_session_id.to_owned()),
            true,
        )
        .await
    }
    async fn spawn_selected(
        options: LaunchOptions,
        servers: Vec<String>,
        create_id: Option<String>,
        without_tools: bool,
    ) -> Result<Self, Error> {
        let native_servers = sdk_server_configuration(servers)?;
        let sdk_servers = native_servers.keys().cloned().collect();
        let (auth, native_version) = crate::auth::probed_auth_status(&options).await?;
        if !auth.logged_in {
            return Err(Error::InvalidConfiguration(
                "native authentication required",
            ));
        }
        let expected_session_id = create_id
            .clone()
            .or_else(|| options.resume_session_id.clone());
        let mut command = options.stream_command();
        if without_tools {
            configure_without_tools(&mut command);
        }
        if let Some(id) = create_id {
            command.arg(format!("--session-id={id}"));
        }
        if !native_servers.is_empty() {
            command.arg(format!(
                "--mcp-config={}",
                json!({"mcpServers":native_servers})
            ));
        }
        let mut child = command.spawn()?;
        let stdin = child.stdin.take().ok_or(Error::Closed)?;
        let stdout = child.stdout.take().ok_or(Error::Closed)?;
        let stderr = child.stderr.take().ok_or(Error::Closed)?;
        let (event_tx, events) = event_channel(options.limits.queued_event_bytes);
        let (write_tx, write_rx) = mpsc::channel(options.limits.queued_writes);
        let (stop, stopped) = watch::channel(false);
        let shared = Arc::new(Shared {
            model_selection_contract: crate::models::ModelSelectionContract::for_version(
                &native_version,
            ),
            native_version,
            reaped: watch::channel(ReapState::Pending).0,
            state: Mutex::new(State {
                pending: BTreeMap::new(),
                callbacks: BTreeMap::new(),
                responding: BTreeSet::new(),
                lifecycle: Lifecycle::new(options.settlement_support, options.limits.active_tasks),
                attribution: crate::attribution::AttributionLedger::new(),
                attributed_mode: None,
                fault: None,
                expected_session_id,
                confirmed_session_id: None,
                initialization_request_id: None,
                skipped_tools: BTreeSet::new(),
            }),
            stop,
            writes: write_tx,
            counter: AtomicU64::new(1),
            initialized: AtomicU8::new(0),
            identity_changed: Notify::new(),
            limits: options.limits,
            sdk_servers,
            sdk_metadata: std::array::from_fn(|_| AtomicU64::new(0)),
        });
        let reader_shared = Arc::clone(&shared);
        let reader = tokio::spawn(async move {
            read_stdout(stdout, reader_shared, event_tx).await;
        });
        // Stderr is intentionally drained without retaining secret-bearing diagnostics.
        let err_shared = Arc::clone(&shared);
        let stderr_reader = tokio::spawn(async move {
            if drain(stderr).await.is_err() {
                err_shared.fail(Fault::Closed);
            }
        });
        let writer_shared = Arc::clone(&shared);
        let writer = tokio::spawn(async move {
            write_stdin(stdin, write_rx, writer_shared).await;
        });
        let supervisor = supervise_process(child, stopped, reader, stderr_reader, writer, &shared);
        Ok(Self {
            client: ClaudeClient { shared },
            events,
            supervisor: Some(supervisor),
        })
    }
    #[must_use]
    pub fn client(&self) -> ClaudeClient {
        self.client.clone()
    }
    #[must_use]
    pub fn settlement(&self) -> Settlement {
        self.client.settlement()
    }
    /// Receive bounded observations. No callbacks block the native reader.
    /// # Errors
    /// Capacity/protocol faults terminate transport, while preserving uncertainty.
    pub async fn next_event(&mut self) -> Result<Event, Error> {
        match self.events.recv().await {
            Some(event) => Ok(event),
            None => Err(self
                .client
                .shared
                .lock()
                .fault
                .unwrap_or(Fault::Closed)
                .error()),
        }
    }
    /// Stop and await supervisor-observed physical reap, unlike protocol Close acknowledgement.
    /// # Errors
    /// Returns `Closed` when process release cannot be proved within the request deadline.
    pub async fn close_reaped(mut self) -> Result<PhysicalReap, Error> {
        self.client.terminate();
        let proof = self.client.wait_reaped().await?;
        if let Some(supervisor) = self.supervisor.take() {
            supervisor.await.map_err(|_| Error::Closed)?;
        }
        Ok(proof)
    }

    /// Kill and reap the process. Cancellation is never reported as proof of no effects.
    pub async fn close(mut self) {
        self.client.terminate();
        if let Some(supervisor) = self.supervisor.take() {
            let _ = supervisor.await;
        }
    }
}
// Own process lifetime and pipe cleanup together; publish physical release only after join.
fn supervise_process(
    mut child: tokio::process::Child,
    mut stopped: watch::Receiver<bool>,
    mut reader: JoinHandle<()>,
    mut stderr_reader: JoinHandle<()>,
    writer: JoinHandle<()>,
    shared: &Arc<Shared>,
) -> JoinHandle<()> {
    let supervisor_shared = Arc::clone(shared);
    let supervisor = tokio::spawn(async move {
        let waited = tokio::select! {
            result = child.wait() => result,
            _ = stopped.changed() => {
                let _ = child.start_kill();
                child.wait().await
            },
        };
        // Abort writes on exit but drain final buffered observations before terminal uncertainty.
        writer.abort();
        let readers = async {
            let _ = (&mut reader).await;
            let _ = (&mut stderr_reader).await;
        };
        let _ = tokio::time::timeout(Duration::from_secs(2), readers).await;
        supervisor_shared.fail(Fault::Closed);
        reader.abort();
        stderr_reader.abort();
        waited.is_ok()
    });
    // Only the join observer publishes proof: native Ack, signal delivery and
    // an OS wait before supervisor cleanup are insufficient.
    let completion_shared = Arc::clone(shared);
    tokio::spawn(async move {
        let completed = matches!(supervisor.await, Ok(true));
        let _ = completion_shared.reaped.send_replace(if completed {
            ReapState::Reaped
        } else {
            ReapState::Failed
        });
    })
}

impl Drop for ClaudeSession {
    fn drop(&mut self) {
        self.client.terminate();
    }
}
impl ClaudeClient {
    pub(crate) fn model_selection_contract(&self) -> crate::models::ModelSelectionContract {
        self.shared.model_selection_contract
    }

    /// Probed native `--version` output of this process launch, which may differ from the
    /// configured expected version. Use [`crate::is_tested_version`] before relying on
    /// version-specific verified behavior.
    #[must_use]
    pub fn native_version(&self) -> &str {
        &self.shared.native_version
    }

    pub(crate) fn is_before_first_prompt(&self) -> bool {
        self.is_control_ready() && self.shared.lock().attributed_mode.is_none()
    }

    #[must_use]
    pub fn settlement(&self) -> Settlement {
        self.shared.lock().settlement()
    }
    /// Current live transport admission for a subsequent foreground turn.
    /// Background work need not be settled. This is not host execution authority,
    /// a conditional native start, or a guarantee against future native wake work.
    #[must_use]
    pub fn can_submit_next_turn(&self) -> bool {
        let state = self.shared.lock();
        state.fault.is_none()
            && state.callbacks.values().all(is_mcp_catalog)
            && state.lifecycle.can_submit()
    }
    /// Release process-local attribution only after host-acknowledged terminal and
    /// associated-work evidence. Does not authorize replay; the host retains durable
    /// submission fencing beyond the bounded recent-duplicate window.
    pub fn retire_acknowledged_submission(&self, uuid: &crate::SubmissionUuid) {
        self.shared.lock().attribution.retire(uuid);
    }
    /// Whether the control handshake succeeded and the process has no observed fault.
    /// Does not imply identity confirmation, settlement, or durable resume.
    #[must_use]
    pub fn is_control_ready(&self) -> bool {
        self.shared.initialized.load(Ordering::Acquire) == 2 && self.shared.lock().fault.is_none()
    }
    /// Inspect identity evidence without sending requests or waiting for a first prompt.
    #[must_use]
    pub fn identity_state(&self) -> IdentityState {
        let state = self.shared.lock();
        if state.fault.is_some() {
            return IdentityState::Uncertain;
        }
        let identity = state.confirmed_session_id.as_ref().map_or_else(
            || {
                state
                    .expected_session_id
                    .as_ref()
                    .map_or(IdentityState::Unselected, |id| IdentityState::Prepared {
                        requested_session_id: id.clone(),
                    })
            },
            |id| IdentityState::Confirmed {
                native_session_id: id.clone(),
            },
        );
        drop(state);
        identity
    }
    /// Wait for supervisor-confirmed physical process release without issuing a stop.
    /// # Errors
    /// Returns `Closed` if wait fails or the configured request deadline expires.
    /// Failure never proves release or absence of side effects.
    pub async fn wait_reaped(&self) -> Result<PhysicalReap, Error> {
        let mut reaped = self.shared.reaped.subscribe();
        let wait = async {
            loop {
                let state = *reaped.borrow_and_update();
                match state {
                    ReapState::Reaped => return Ok(PhysicalReap { _private: () }),
                    ReapState::Failed => return Err(Error::Closed),
                    ReapState::Pending => {}
                }
                reaped.changed().await.map_err(|_| Error::Closed)?;
            }
        };
        tokio::time::timeout(self.shared.limits.request_timeout, wait)
            .await
            .map_err(|_| Error::Closed)?
    }

    /// Hard stop; use `interrupt` for the native cooperative control operation.
    pub fn terminate(&self) {
        self.shared.fail(Fault::Closed);
    }
    /// Negotiate once. Failed or abandoned initialization is not silently retried.
    /// # Errors
    /// Returns a configuration error for repeated initialization, transport/protocol error otherwise.
    pub async fn initialize(&self) -> Result<Value, Error> {
        self.request(ControlRequest::Initialize { hooks: None })
            .await?
            .wait()
            .await
    }
    /// Initialize and return only a native-confirmed identity, never a requested-ID echo.
    /// Identity may be supplied by the initialize response or a top-level system init frame.
    /// Ordinary new-session CLI initialization does not report identity before a real
    /// submission. Do not use this strict convenience API to block prepared open.
    /// For ordinary lifecycle use `initialize`, authorized `submit`, then identity observation.
    /// # Errors
    /// Initialization error, native identity mismatch, or missing confirmation. A failure
    /// terminates this process; the caller must reconcile instead of replaying creation.
    pub async fn initialize_session(&self) -> Result<InitializedSession, Error> {
        let result = self.initialize().await;
        if let Err(error) = result {
            self.terminate();
            return Err(error);
        }
        self.wait_session_identity().await
    }
    /// Wait for confirmed identity after custom control initialization (for example hooks).
    /// Does not consume the observation queue or send a native request.
    /// # Errors
    /// Uninitialized transport, missing identity within the bound, or contradictory identity.
    pub async fn wait_session_identity(&self) -> Result<InitializedSession, Error> {
        if self.shared.initialized.load(Ordering::Acquire) != 2 {
            return Err(Error::InvalidConfiguration(
                "initialize before identity confirmation",
            ));
        }
        let wait = async {
            loop {
                // Enable before inspecting under the mutex: do not lose a concurrent marker.
                let notified = self.shared.identity_changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                {
                    let state = self.shared.lock();
                    if let Some(fault) = state.fault {
                        return Err(fault.error());
                    }
                    if let Some(id) = &state.confirmed_session_id {
                        return Ok(InitializedSession {
                            native_session_id: id.clone(),
                        });
                    }
                }
                notified.await;
            }
        };
        match tokio::time::timeout(self.shared.limits.request_timeout, wait).await {
            Ok(Ok(identity)) => Ok(identity),
            Ok(Err(error)) => {
                self.terminate();
                Err(error)
            }
            Err(_) => {
                self.shared.fail(Fault::SessionIdentity);
                Err(Error::SessionIdentity)
            }
        }
    }
    /// Send a correlated control request without blocking other requests/callbacks.
    /// # Errors
    /// Capacity, invalid protocol state, or uncertain transport delivery.
    pub async fn request(&self, request: ControlRequest) -> Result<ControlTicket, Error> {
        let initialization = matches!(request, ControlRequest::Initialize { .. });
        if initialization {
            self.shared
                .initialized
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .map_err(|_| Error::InvalidConfiguration("initialization already attempted"))?;
        } else if self.shared.initialized.load(Ordering::Acquire) != 2 {
            return Err(Error::InvalidConfiguration(
                "initialize before control operations",
            ));
        }
        let mut bound = BoundedFrame {
            bytes: Vec::new(),
            limit: self.shared.limits.frame_bytes,
        };
        serde_json::to_writer(&mut bound, &request).map_err(|_| Error::Capacity)?;
        let mut native_request: Value =
            serde_json::from_slice(&bound.bytes).map_err(|_| Error::Protocol)?;
        add_sdk_registration(
            &mut native_request,
            initialization,
            &self.shared.sdk_servers,
        );
        self.request_wire(native_request, initialization).await
    }

    async fn request_wire(
        &self,
        native_request: Value,
        initialization: bool,
    ) -> Result<ControlTicket, Error> {
        let id = format!(
            "rust_{}",
            self.shared.counter.fetch_add(1, Ordering::Relaxed)
        );
        let value = json!({"type":"control_request","request_id":id,"request":native_request});
        let (tx, rx) = oneshot::channel();
        let ack = {
            let mut state = self.shared.lock();
            if let Some(fault) = state.fault {
                return Err(fault.error());
            }
            if state.pending.len() >= self.shared.limits.pending_requests {
                return Err(Error::Capacity);
            }
            let ack = self.shared.enqueue(&value, None)?;
            if initialization {
                state.initialization_request_id = Some(id.clone());
            }
            state.pending.insert(id.clone(), tx);
            ack
        };
        let ticket = ControlTicket {
            id,
            response: Some(rx),
            shared: Arc::clone(&self.shared),
            initialization,
        };
        if matches!(
            tokio::time::timeout(self.shared.limits.request_timeout, ack).await,
            Ok(Ok(Ok(())))
        ) {
            Ok(ticket)
        } else {
            self.shared.fail(Fault::Closed);
            Err(Error::Uncertain)
        }
    }
    /// Submit text on the initialized stream; no native acceptance is inferred from writing.
    /// # Errors
    /// Closed/uninitialized transport, oversized frame, full queue, or uncertain delivery.
    pub async fn submit(
        &self,
        text: &str,
        native_session_id: Option<&str>,
    ) -> Result<DeliveryReceipt, Error> {
        self.submit_inner(text, native_session_id, None).await
    }
    /// Submit one exact client-selected attempt UUID, retained before enqueueing.
    /// The UUID is a correlation key, never a native turn ID. A failed/uncertain
    /// write does not release it or authorize retry. At most 256 live/recent UUIDs
    /// are retained; explicit acknowledged retirement permits longer sessions.
    /// Callers must not mix attributed and legacy sends.
    /// # Errors
    /// Same as `submit`, plus reused UUID or exhausted retained-attempt capacity.
    pub async fn submit_with_uuid(
        &self,
        text: &str,
        native_session_id: Option<&str>,
        uuid: crate::SubmissionUuid,
    ) -> Result<DeliveryReceipt, Error> {
        self.submit_inner(text, native_session_id, Some(uuid)).await
    }
    /// Read native MCP status and return only the host-tools connection category.
    /// Raw native errors, server URLs and account metadata are never returned.
    /// # Errors
    /// Uninitialized/closed transport, unsupported native operation or bounded timeout.
    pub async fn host_tools_status(&self) -> Result<&'static str, Error> {
        if !self.is_control_ready() {
            return Err(Error::InvalidConfiguration("not initialized"));
        }
        let response = self
            .request_wire(json!({"subtype":"mcp_status"}), false)
            .await?
            .wait()
            .await?;
        Ok(host_tools_connection(&response).label())
    }

    /// Saturating counts of startup initialize, startup empty catalog and live catalog requests.
    /// No native content or identifiers are retained.
    #[must_use]
    pub fn sdk_metadata_counts(&self) -> [u64; 3] {
        std::array::from_fn(|index| self.shared.sdk_metadata[index].load(Ordering::Relaxed))
    }

    /// Deliver catalog-change notifications using retained, bounded control requests.
    /// Acknowledgement proves receipt only, not catalog discovery. Call outside service/queue locks.
    /// # Errors
    /// Returns sanitized rejection, closed transport, capacity or uncertain timeout. Never retries.
    pub async fn refresh_sdk_catalogs_acknowledged(&self) -> Result<(), Error> {
        if !self.is_control_ready() {
            return Err(Error::InvalidConfiguration("not initialized"));
        }
        for server in &self.shared.sdk_servers {
            self.request_wire(
                json!({"subtype":"mcp_message", "server_name":server,
                "message":{"jsonrpc":"2.0", "method":"notifications/tools/list_changed"}}),
                false,
            )
            .await?
            .wait()
            .await?;
        }
        Ok(())
    }

    /// Queue metadata rediscovery after host binding, before the next input frame.
    /// Never waits for native discovery or callbacks; callers may hold a plugin service
    /// lock, but must release their queue mutex before calling. This grants no authority.
    /// # Errors
    /// Closed transport or bounded write-queue capacity pressure.
    pub fn refresh_sdk_catalogs(&self) -> Result<(), Error> {
        if !self.is_control_ready() {
            return Err(Error::InvalidConfiguration("not initialized"));
        }
        for server in &self.shared.sdk_servers {
            let id = format!(
                "rust_{}",
                self.shared.counter.fetch_add(1, Ordering::Relaxed)
            );
            let message = sdk_catalog_changed(&id, server);
            let _written = self.shared.enqueue(&message, None)?;
        }
        Ok(())
    }

    async fn submit_inner(
        &self,
        text: &str,
        native_session_id: Option<&str>,
        uuid: Option<crate::SubmissionUuid>,
    ) -> Result<DeliveryReceipt, Error> {
        if self.shared.initialized.load(Ordering::Acquire) != 2 {
            return Err(Error::InvalidConfiguration("not initialized"));
        }
        if text.len() > self.shared.limits.frame_bytes {
            return Err(Error::Capacity);
        }
        if let Some(id) = native_session_id {
            crate::launch::validate_id(id)?;
            let state = self.shared.lock();
            if state
                .confirmed_session_id
                .as_deref()
                .or(state.expected_session_id.as_deref())
                != Some(id)
            {
                return Err(Error::SessionIdentity);
            }
        }
        let mut frame = json!({"type":"user","session_id":native_session_id.unwrap_or(""),
            "message":{"role":"user","content":text},"parent_tool_use_id":null});
        if let Some(uuid) = &uuid {
            frame["uuid"] = json!(uuid.as_str());
        }
        let ack = {
            let mut state = self.shared.lock();
            if let Some(fault) = state.fault {
                return Err(fault.error());
            }
            if !state.lifecycle.can_submit() || !state.callbacks.values().all(is_mcp_catalog) {
                return Err(Error::InvalidConfiguration(
                    "active turn steering is unsupported; wait for the current foreground result",
                ));
            }
            if state
                .attributed_mode
                .is_some_and(|mode| mode != uuid.is_some())
            {
                return Err(Error::InvalidConfiguration(
                    "mixed attributed and legacy submissions",
                ));
            }
            state.attributed_mode = Some(uuid.is_some());
            if let Some(uuid) = uuid {
                state.attribution.retain(uuid)?;
            }
            let ack = self.shared.enqueue(&frame, None)?;
            state.lifecycle.submit();
            ack
        };
        wait_write(&self.shared, ack).await
    }
    /// Respond once to a still-live callback. Cancellation and response admission are serialized.
    /// # Errors
    /// Stale identity, mismatched response kind, bound exceeded or uncertain delivery.
    pub async fn respond(
        &self,
        request_id: &str,
        response: CallbackResponse,
    ) -> Result<DeliveryReceipt, Error> {
        let ack = {
            let mut state = self.shared.lock();
            if let Some(fault) = state.fault {
                return Err(fault.error());
            }
            if state.responding.contains(request_id) {
                return Err(Error::StaleCallback);
            }
            let request = state
                .callbacks
                .get(request_id)
                .ok_or(Error::StaleCallback)?;
            let body = if matches!(response, CallbackResponse::Error) {
                json!({"subtype":"error","request_id":request_id,"error":"host rejected callback"})
            } else {
                json!({"subtype":"success","request_id":request_id,"response":response.payload(request)?})
            };
            let ack = self.shared.enqueue(
                &json!({"type":"control_response","response":body}),
                Some(request_id.to_owned()),
            )?;
            state.responding.insert(request_id.to_owned());
            ack
        };
        wait_write(&self.shared, ack).await
    }
    /// Cooperative interruption ack does not establish settlement or rollback.
    /// # Errors
    /// Same errors as `request`/`ControlTicket::wait`.
    pub async fn interrupt(&self) -> Result<Value, Error> {
        self.request(ControlRequest::Interrupt).await?.wait().await
    }
}
async fn wait_write(
    shared: &Shared,
    ack: oneshot::Receiver<Result<(), Error>>,
) -> Result<DeliveryReceipt, Error> {
    match tokio::time::timeout(shared.limits.request_timeout, ack).await {
        Ok(Ok(Ok(()))) => Ok(DeliveryReceipt {
            bytes_written: true,
        }),
        Ok(Ok(Err(Error::StaleCallback))) => Err(Error::StaleCallback),
        _ => {
            shared.fail(Fault::Closed);
            Err(Error::Uncertain)
        }
    }
}
async fn write_stdin(
    mut stdin: tokio::process::ChildStdin,
    mut writes: mpsc::Receiver<Write>,
    shared: Arc<Shared>,
) {
    while let Some(write) = writes.recv().await {
        if let Some(id) = &write.callback {
            // Suppress queued responses invalidated by native cancellation before delivery starts.
            let active = {
                let mut state = shared.lock();
                state.responding.remove(id) && state.callbacks.remove(id).is_some()
            };
            if !active {
                let _ = write.ack.send(Err(Error::StaleCallback));
                continue;
            }
        }
        let result = async {
            stdin.write_all(&write.frame).await?;
            stdin.flush().await
        }
        .await;
        if result.is_ok() {
            let _ = write.ack.send(Ok(()));
        } else {
            let _ = write.ack.send(Err(Error::Uncertain));
            shared.fail(Fault::Closed);
            break;
        }
    }
}
async fn drain(mut reader: impl AsyncRead + Unpin) -> std::io::Result<()> {
    let mut buffer = [0_u8; 8192];
    while reader.read(&mut buffer).await? != 0 {}
    Ok(())
}
async fn read_stdout(
    mut stdout: tokio::process::ChildStdout,
    shared: Arc<Shared>,
    events: EventSender,
) {
    let mut chunk = [0_u8; 8192];
    let mut frame = Vec::new();
    // An oversized line is skipped to its end; the session continues after it.
    let mut skipping = false;
    let mut failed = false;
    loop {
        let count = match stdout.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(count) => count,
        };
        for byte in &chunk[..count] {
            if failed {
                continue;
            }
            if *byte == b'\n' {
                if skipping {
                    skipping = false;
                    failed = fault_on(
                        &shared,
                        send_event(&events, skipped_frame("oversized_frame", None), 64),
                    );
                    continue;
                }
                if frame.is_empty() {
                    continue;
                }
                let bytes = frame.len();
                let result = crate::attribution::decode_frame(&frame).map_or_else(
                    |_| send_event(&events, skipped_frame("unreadable_frame", None), 64),
                    |value| route_frame(value, bytes, &shared, &events),
                );
                frame.clear();
                failed = fault_on(&shared, result);
            } else if skipping {
                // Discard the rest of the oversized line.
            } else if frame.len() >= shared.limits.frame_bytes {
                frame.clear();
                skipping = true;
            } else {
                frame.push(*byte);
            }
        }
    }
    shared.fail(if frame.is_empty() {
        Fault::Closed
    } else {
        Fault::Protocol
    });
}

/// Apply a routing failure; returns whether the transport faulted. Only a native identity
/// mismatch or a consumer that stopped draining is a fault.
fn fault_on(shared: &Shared, result: Result<(), Error>) -> bool {
    let Err(error) = result else {
        return false;
    };
    shared.fail(match error {
        Error::SessionIdentity => Fault::SessionIdentity,
        Error::Capacity => Fault::Capacity,
        _ => Fault::Protocol,
    });
    true
}
fn configure_without_tools(command: &mut tokio::process::Command) {
    command.args([
        "--tools",
        "",
        "--strict-mcp-config",
        "--mcp-config",
        "{\"mcpServers\":{}}",
        "--disable-slash-commands",
        "--no-session-persistence",
    ]);
}
fn sdk_server_configuration(servers: Vec<String>) -> Result<BTreeMap<String, Value>, Error> {
    if servers.len() > 32 {
        return Err(Error::Capacity);
    }
    let mut native_servers = BTreeMap::new();
    for name in servers {
        crate::launch::validate_id(&name)?;
        if native_servers
            .insert(name.clone(), json!({"type":"sdk","name":name}))
            .is_some()
        {
            return Err(Error::InvalidConfiguration("duplicate SDK MCP server"));
        }
    }
    Ok(native_servers)
}
fn send_event(events: &EventSender, event: Event, bytes: usize) -> Result<(), Error> {
    events.send(event, bytes)
}
fn confirm_identity(state: &mut State, id: &str) -> Result<(), Error> {
    crate::launch::validate_id(id)?;
    if state
        .expected_session_id
        .as_deref()
        .is_some_and(|expected| expected != id)
        || state
            .confirmed_session_id
            .as_deref()
            .is_some_and(|confirmed| confirmed != id)
    {
        return Err(Error::SessionIdentity);
    }
    state.confirmed_session_id = Some(id.to_owned());
    Ok(())
}
fn handle_control_response(value: &Value, shared: &Shared, state: &mut State) -> Result<(), Error> {
    let body = value.get("response").ok_or(Error::Protocol)?;
    let id = body
        .get("request_id")
        .and_then(Value::as_str)
        .ok_or(Error::Protocol)?;
    let response = match body.get("subtype").and_then(Value::as_str) {
        Some("success") => Ok(body.get("response").cloned().unwrap_or_else(|| json!({}))),
        Some("error") => Err(Error::Protocol),
        _ => return Err(Error::Protocol),
    };
    if state.initialization_request_id.as_deref() == Some(id)
        && let Ok(payload) = &response
        && let Some(identity) = payload.get("session_id")
    {
        confirm_identity(state, identity.as_str().ok_or(Error::SessionIdentity)?)?;
        shared.identity_changed.notify_waiters();
    }
    if let Some(tx) = state.pending.remove(id) {
        let _ = tx.send(response);
    }
    Ok(())
}
fn is_mcp_catalog(request: &CallbackRequest) -> bool {
    matches!(request, CallbackRequest::McpMessage { message, .. }
        if message["jsonrpc"] == "2.0" && message["method"] == "tools/list")
}

fn sdk_catalog_changed(id: &str, server: &str) -> Value {
    json!({"type":"control_request", "request_id":id,
        "request":{"subtype":"mcp_message", "server_name":server,
            "message":{"jsonrpc":"2.0", "method":"notifications/tools/list_changed"}}})
}

fn add_sdk_registration(request: &mut Value, initializing: bool, servers: &BTreeSet<String>) {
    if initializing && !servers.is_empty() {
        request["sdkMcpServers"] = json!(servers);
    }
}

fn record_sdk_metadata(request: &CallbackRequest, shared: &Shared) {
    let CallbackRequest::McpMessage {
        server_name,
        message,
    } = request
    else {
        return;
    };
    if !shared.sdk_servers.contains(server_name) || message["jsonrpc"] != "2.0" {
        return;
    }
    let initializing = shared.initialized.load(Ordering::Acquire) != 2;
    let index = match message["method"].as_str() {
        Some("initialize") => 0,
        Some("tools/list") if initializing => 1,
        Some("tools/list") => 2,
        _ => return,
    };
    let _ = shared.sdk_metadata[index].try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        Some(value.saturating_add(1))
    });
}

fn sdk_startup_response(request: &CallbackRequest, initializing: bool) -> Option<Value> {
    let CallbackRequest::McpMessage { message, .. } = request else {
        return None;
    };
    if message["jsonrpc"] != "2.0" {
        return None;
    }
    let result = match message["method"].as_str()? {
        "initialize" => json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {"listChanged": true}},
            "serverInfo": {"name": "bcode-sdk", "version": "1.0.0"},
        }),
        "ping" | "notifications/initialized" => json!({}),
        "tools/list" if initializing => json!({"tools": []}),
        _ => return None,
    };
    // CLI stream-json acknowledges notifications with an empty result as well.
    let id = message.get("id").cloned().unwrap_or_else(|| json!(0));
    Some(json!({"mcp_response": {"jsonrpc":"2.0", "id":id, "result":result}}))
}

/// A frame the adapter could not use, relayed so the host can see it was skipped. Carries
/// only the frame's type and a fixed reason code, never native content.
fn skipped_frame(code: &'static str, value: Option<&Value>) -> Event {
    let field = |name: &str| {
        value
            .and_then(|value| value.get(name))
            .and_then(Value::as_str)
            .filter(|text| text.len() <= 64 && text.chars().all(|c| c.is_ascii_graphic()))
            .map(ToOwned::to_owned)
    };
    Event::Message(NativeMessage::Notice(json!({
        "type": crate::ADAPTER_NOTICE_TYPE,
        "code": code,
        "frame_type": field("type"),
        "frame_subtype": field("subtype"),
    })))
}

fn handle_control_request(
    value: &Value,
    shared: &Shared,
    state: &mut State,
    events: &EventSender,
    bytes: usize,
) -> Result<(), Error> {
    let Some(id) = value
        .get("request_id")
        .and_then(Value::as_str)
        .filter(|id| crate::launch::validate_id(id).is_ok())
    else {
        return send_event(
            events,
            skipped_frame("unreadable_control_request", Some(value)),
            64,
        );
    };
    let request = value
        .get("request")
        .cloned()
        .and_then(|request| serde_json::from_value::<CallbackRequest>(request).ok());
    if state.callbacks.contains_key(id) {
        // A repeat of a request already being answered: answering it again would answer both.
        return send_event(
            events,
            skipped_frame("duplicate_control_request", Some(value)),
            64,
        );
    }
    let refusal = if state.callbacks.len() >= shared.limits.active_callbacks {
        "too_many_control_requests"
    } else {
        "unsupported_control_request"
    };
    // A request the adapter cannot serve is refused, so the native agent never waits on it.
    let Some(request) = request.filter(|_| state.callbacks.len() < shared.limits.active_callbacks)
    else {
        let _ = shared.enqueue(&control_error(id), None);
        return send_event(events, skipped_frame(refusal, value.get("request")), 64);
    };
    record_sdk_metadata(&request, shared);
    if let CallbackRequest::McpMessage { server_name, .. } = &request
        && shared.sdk_servers.contains(server_name)
        && let Some(payload) =
            sdk_startup_response(&request, shared.initialized.load(Ordering::Acquire) != 2)
    {
        let response = json!({"type":"control_response","response":{"subtype":"success","request_id":id,"response":payload}});
        // Initialization must not depend on a plugin drain that starts only after Open.
        // Only protocol negotiation and an empty pre-grant catalog are handled here.
        let _written = shared.enqueue(&response, None)?;
        return Ok(());
    }
    if !is_mcp_catalog(&request) {
        state.lifecycle.attention();
    }
    state.callbacks.insert(id.to_owned(), request.clone());
    send_event(
        events,
        Event::Callback {
            request_id: id.to_owned(),
            request,
        },
        bytes,
    )
}

/// Remember the tools a skipped frame started, within a fixed bound.
fn remember_skipped_tools(state: &mut State, value: &Value) {
    const MAX_SKIPPED_TOOLS: usize = 256;
    let tools = value
        .pointer("/message/content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
        .filter_map(|block| block.get("id").and_then(Value::as_str));
    for tool in tools {
        if state.skipped_tools.len() < MAX_SKIPPED_TOOLS {
            state.skipped_tools.insert(tool.to_owned());
        }
    }
}

/// A refusal of native control request `id`.
fn control_error(id: &str) -> Value {
    json!({"type":"control_response","response":{"subtype":"error","request_id":id,
        "error":"unsupported by this host"}})
}

#[cfg(test)]
fn route(value: Value, shared: &Shared, events: &EventSender) -> Result<(), Error> {
    route_frame(value, 512, shared, events)
}

/// Route one native frame of `bytes` bytes. Only a native session identity mismatch and a
/// consumer that stopped draining are faults; a frame the adapter does not understand is
/// relayed as a notice and the session continues.
fn route_frame(
    value: Value,
    bytes: usize,
    shared: &Shared,
    events: &EventSender,
) -> Result<(), Error> {
    let Some(kind) = value
        .get("type")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
    else {
        return send_event(events, skipped_frame("untyped_frame", None), 64);
    };
    let mut state = shared.lock();
    if state.fault.is_some() {
        return Ok(());
    }
    match kind.as_str() {
        "control_response" => {
            if handle_control_response(&value, shared, &mut state).is_err() {
                drop(state);
                send_event(
                    events,
                    skipped_frame("unreadable_control_response", Some(&value)),
                    64,
                )?;
            }
        }
        "control_request" => {
            handle_control_request(&value, shared, &mut state, events, bytes)?;
        }
        "control_cancel_request" => {
            let Some(id) = value.get("request_id").and_then(Value::as_str) else {
                drop(state);
                return send_event(events, skipped_frame("unreadable_control_cancel", None), 64);
            };
            state.responding.remove(id);
            if state.callbacks.remove(id).is_some() {
                send_event(
                    events,
                    Event::CallbackCancelled {
                        request_id: id.to_owned(),
                    },
                    64,
                )?;
            }
        }
        "system" | "assistant" | "user" | "result" | "stream_event" => {
            route_message(value, &kind, bytes, shared, state, events)?;
        }
        // Informational frames (progress, keep-alive, summaries, rate limits) and frame types
        // newer than this adapter carry nothing it must act on: relayed, never fatal.
        _ => {
            if let Some(id) = value.get("session_id").and_then(Value::as_str)
                && value.get("parent_tool_use_id").is_none_or(Value::is_null)
                && state
                    .confirmed_session_id
                    .as_deref()
                    .is_some_and(|confirmed| confirmed != id)
            {
                return Err(Error::SessionIdentity);
            }
            drop(state);
            send_event(events, Event::Message(NativeMessage::Notice(value)), bytes)?;
        }
    }
    Ok(())
}

fn route_message(
    value: Value,
    kind: &str,
    bytes: usize,
    shared: &Shared,
    mut state: MutexGuard<'_, State>,
    events: &EventSender,
) -> Result<(), Error> {
    if kind == "system" && value.get("subtype").and_then(Value::as_str) == Some("init") {
        let id = value
            .get("session_id")
            .and_then(Value::as_str)
            .ok_or(Error::SessionIdentity)?;
        confirm_identity(&mut state, id)?;
        shared.identity_changed.notify_waiters();
    } else if value.get("parent_tool_use_id").is_none_or(Value::is_null)
        && let Some(id) = value.get("session_id")
    {
        let id = id.as_str().ok_or(Error::SessionIdentity)?;
        if state
            .confirmed_session_id
            .as_deref()
            .is_some_and(|confirmed| confirmed != id)
            || state
                .expected_session_id
                .as_deref()
                .is_some_and(|expected| expected != id)
        {
            return Err(Error::SessionIdentity);
        }
    }
    // A frame whose submission cannot be correlated (one already retired, say) is skipped,
    // with its tools' task frames, so it is never credited to a later submission.
    let attribution = match state.attribution.observe_delivery(&value) {
        Ok(attribution) => attribution,
        Err(Error::SessionIdentity) => return Err(Error::SessionIdentity),
        Err(_) => {
            remember_skipped_tools(&mut state, &value);
            drop(state);
            return send_event(
                events,
                skipped_frame("uncorrelated_frame", Some(&value)),
                64,
            );
        }
    };
    if value
        .get("tool_use_id")
        .and_then(Value::as_str)
        .is_some_and(|tool| state.skipped_tools.contains(tool))
    {
        drop(state);
        return send_event(
            events,
            skipped_frame("uncorrelated_frame", Some(&value)),
            64,
        );
    }
    let mut notices = Vec::new();
    if attribution.is_some() {
        let id = value
            .get("session_id")
            .and_then(Value::as_str)
            .ok_or(Error::SessionIdentity)?;
        confirm_identity(&mut state, id)?;
        shared.identity_changed.notify_waiters();
    }
    let previous = state.settlement();
    // A session-scoped/startup/stale result cannot settle an attributed attempt.
    if (kind != "result"
        || state.attributed_mode != Some(true)
        || state.attribution.answers_current(attribution.as_ref()))
        && state.lifecycle.observe(&value).is_err()
    {
        // A lifecycle shape this adapter does not know leaves settlement where it was.
        notices.push(skipped_frame("unreadable_lifecycle", Some(&value)));
    }
    let next = state.settlement();
    drop(state);
    let message = match kind {
        "system" => NativeMessage::System(value),
        "assistant" => NativeMessage::Assistant(value),
        "user" => NativeMessage::User(value),
        "result" => NativeMessage::Result(value),
        _ => NativeMessage::Stream(value),
    };
    for notice in notices {
        send_event(events, notice, 64)?;
    }
    if let Some(attribution) = attribution {
        send_event(events, Event::Attribution(attribution), 512)?;
    }
    send_event(events, Event::Message(message), bytes)?;
    if next != previous {
        send_event(events, Event::Settlement(next), 64)?;
    }
    Ok(())
}

#[cfg(test)]
mod sdk_handshake_tests {
    use super::*;

    #[test]
    fn native_mcp_status_exposes_only_typed_connection_category() {
        assert_eq!(host_tools_connection(&json!({"mcpServers":[{
            "name":"bcode-tools", "status":"connected", "error":"DO_NOT_EXPORT", "config":{"url":"SECRET"},
        }]})).label(), "connected");
        assert_eq!(
            host_tools_connection(&json!({"mcpServers":[]})).label(),
            "missing"
        );
        assert_eq!(
            host_tools_connection(
                &json!({"mcpServers":[{"name":"bcode-tools","status":"future"}]})
            )
            .label(),
            "unknown"
        );
        assert_eq!(
            host_tools_connection(
                &json!({"mcpServers":[{"name":"bcode-tools","status":"failed"}]})
            )
            .label(),
            "failed"
        );
        assert_eq!(
            host_tools_connection(&json!({"unexpected":"SECRET"})).label(),
            "unknown"
        );
    }

    #[test]
    fn initialize_registers_sdk_servers_only_on_negotiation() {
        let servers = BTreeSet::from(["bcode-tools".into()]);
        let mut initialize =
            serde_json::to_value(ControlRequest::Initialize { hooks: None }).unwrap();
        add_sdk_registration(&mut initialize, true, &servers);
        assert_eq!(initialize["sdkMcpServers"], json!(["bcode-tools"]));
        let mut interrupt = serde_json::to_value(ControlRequest::Interrupt).unwrap();
        add_sdk_registration(&mut interrupt, false, &servers);
        assert!(interrupt.get("sdkMcpServers").is_none());
    }

    #[tokio::test]
    async fn termination_signal_is_not_physical_reap_evidence() {
        let (client, _written) = handshake_transport();
        client.terminate();
        let waiting = client.wait_reaped();
        tokio::pin!(waiting);
        tokio::select! {
            biased;
            _ = &mut waiting => panic!("termination is not proof of reap"),
            () = tokio::task::yield_now() => {},
        }
        let _ = client.shared.reaped.send_replace(ReapState::Reaped);
        assert!(waiting.await.is_ok());
        // A later observer sees retained proof; no notification race or second stop.
        assert!(client.wait_reaped().await.is_ok());
    }

    #[tokio::test]
    async fn failed_process_wait_cannot_prove_release() {
        let (client, _written) = handshake_transport();
        let _ = client.shared.reaped.send_replace(ReapState::Failed);
        assert!(client.wait_reaped().await.is_err());
    }

    fn handshake_transport() -> (ClaudeClient, mpsc::Receiver<Write>) {
        let (writes, written) = mpsc::channel(8);
        let (stop, _stopped) = watch::channel(false);
        let shared = Shared {
            model_selection_contract: crate::models::ModelSelectionContract::ExactIdentity,
            native_version: "test-only".into(),
            reaped: watch::channel(ReapState::Pending).0,
            state: Mutex::new(State {
                pending: BTreeMap::new(),
                callbacks: BTreeMap::new(),
                responding: BTreeSet::new(),
                lifecycle: Lifecycle::new(crate::SettlementSupport::VerifiedSessionState, 8),
                attribution: crate::attribution::AttributionLedger::new(),
                attributed_mode: None,
                fault: None,
                expected_session_id: None,
                confirmed_session_id: None,
                initialization_request_id: None,
                skipped_tools: BTreeSet::new(),
            }),
            stop,
            writes,
            counter: AtomicU64::new(1),
            initialized: AtomicU8::new(1),
            identity_changed: Notify::new(),
            limits: crate::Limits::default(),
            sdk_servers: BTreeSet::from(["bcode-tools".into()]),
            sdk_metadata: std::array::from_fn(|_| AtomicU64::new(0)),
        };
        (
            ClaudeClient {
                shared: Arc::new(shared),
            },
            written,
        )
    }

    #[tokio::test]
    async fn unknown_model_selection_never_writes_or_allows_default_prompt() {
        let (client, mut written) = handshake_transport();
        client.shared.initialized.store(2, Ordering::Release);
        let result = client
            .select_model_before_prompt(
                &json!({"models":[{"value":"known","displayName":"Known"}]}),
                "unknown",
            )
            .await;
        assert!(result.is_err());
        assert!(written.try_recv().is_err());
        assert!(!client.is_control_ready());
        assert!(client.submit("must not be delivered", None).await.is_err());
        assert!(written.try_recv().is_err());
    }

    #[tokio::test]
    async fn model_selection_waits_for_exact_control_ack_before_prompt() {
        let (client, mut written) = handshake_transport();
        client.shared.initialized.store(2, Ordering::Release);
        let (events, _received) = event_channel(1 << 20);
        let initialize = json!({"models":[{"value":"known","displayName":"Known"}]});
        let select = client.select_model_before_prompt(&initialize, "known");
        let native = async {
            let write = written.recv().await.unwrap();
            let frame: Value = serde_json::from_slice(&write.frame).unwrap();
            assert_eq!(frame["request"]["subtype"], "set_model");
            assert_eq!(frame["request"]["model"], "known");
            let request_id = frame["request_id"].as_str().unwrap().to_owned();
            write.ack.send(Ok(())).unwrap();
            assert!(written.try_recv().is_err());
            route(
                json!({"type":"control_response","response":{
                    "subtype":"success","request_id":request_id,"response":{}
                }}),
                &client.shared,
                &events,
            )
            .unwrap();
        };
        let (selected, ()) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(select, native)
        })
        .await
        .unwrap();
        let selected = selected.unwrap();
        assert_eq!(selected.requested, "known");
        assert_eq!(selected.effective, None);
        assert!(client.is_control_ready());
    }

    #[tokio::test]
    async fn advertised_alias_effective_model_requires_exact_control_correlation() {
        let (mut client, mut written) = handshake_transport();
        Arc::get_mut(&mut client.shared)
            .unwrap()
            .model_selection_contract =
            crate::models::ModelSelectionContract::AdvertisedAliasResolution;
        client.shared.initialized.store(2, Ordering::Release);
        let (events, _received) = event_channel(1 << 20);
        let initialize = json!({"models":[{"value":"haiku","displayName":"Native alias"}]});
        let select = client.select_model_before_prompt(&initialize, "haiku");
        let native = async {
            let write = written.recv().await.unwrap();
            let frame: Value = serde_json::from_slice(&write.frame).unwrap();
            assert_eq!(frame["request"]["model"], "haiku");
            let request_id = frame["request_id"].as_str().unwrap().to_owned();
            write.ack.send(Ok(())).unwrap();
            // An unrelated success cannot settle this ticket or supply its effective model.
            route(json!({"type":"control_response","response":{
                "subtype":"success","request_id":"unrelated","response":{"model":"claude-other-1"}
            }}), &client.shared, &events).unwrap();
            assert!(client.shared.lock().pending.contains_key(&request_id));
            route(json!({"type":"control_response","response":{
                "subtype":"success","request_id":request_id,"response":{"model":"claude-haiku-5-5"}
            }}), &client.shared, &events).unwrap();
        };
        let (selected, ()) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(select, native)
        })
        .await
        .unwrap();
        let selected = selected.unwrap();
        assert_eq!(selected.requested, "haiku");
        assert_eq!(selected.effective.as_deref(), Some("claude-haiku-5-5"));
        assert!(written.try_recv().is_err());
    }

    #[tokio::test]
    async fn approval_response_allows_correlated_mcp_call_without_replaying_approval() {
        let (client, mut written) = handshake_transport();
        client.shared.initialized.store(2, Ordering::Release);
        let (events, mut received) = event_channel(1 << 20);
        route(
            json!({"type":"control_request","request_id":"approval",
                "request":{"subtype":"can_use_tool","tool_name":"mcp__bcode-tools__filesystem_read",
                "input":{"path":"fixture.txt"},"tool_use_id":"native-read"}}),
            &client.shared,
            &events,
        )
        .unwrap();
        assert!(
            matches!(received.recv().await.unwrap(), Event::Callback { request_id, .. } if request_id == "approval")
        );
        let responding = client.respond(
            "approval",
            CallbackResponse::Allow {
                updated_input: json!({"path":"fixture.txt"}),
            },
        );
        let native = async {
            let write = written.recv().await.unwrap();
            assert_eq!(write.callback.as_deref(), Some("approval"));
            let frame: Value = serde_json::from_slice(&write.frame).unwrap();
            assert_eq!(frame["response"]["request_id"], "approval");
            // Match the writer's one-time callback consumption before acknowledging bytes.
            {
                let mut state = client.shared.lock();
                assert!(state.responding.remove("approval"));
                assert!(state.callbacks.remove("approval").is_some());
                drop(state);
            }
            write.ack.send(Ok(())).unwrap();
            route(
                json!({"type":"control_request","request_id":"mcp-call",
                    "request":{"subtype":"mcp_message","server_name":"bcode-tools",
                    "message":{"jsonrpc":"2.0","id":9,"method":"tools/call",
                    "params":{"name":"filesystem_read","arguments":{"path":"fixture.txt"}}}}}),
                &client.shared,
                &events,
            )
            .unwrap();
        };
        let (delivered, ()) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(responding, native)
        })
        .await
        .unwrap();
        assert!(delivered.unwrap().bytes_written);
        assert!(
            matches!(received.recv().await.unwrap(), Event::Callback { request_id, .. } if request_id == "mcp-call")
        );
        assert!(matches!(
            client.respond("approval", CallbackResponse::Error).await,
            Err(Error::StaleCallback)
        ));
        assert!(written.try_recv().is_err());
    }

    #[tokio::test]
    async fn refresh_timeout_is_unknown_and_does_not_replay() {
        let (mut client, mut written) = handshake_transport();
        Arc::get_mut(&mut client.shared)
            .unwrap()
            .limits
            .request_timeout = Duration::from_millis(10);
        client.shared.initialized.store(2, Ordering::Release);
        let refresh = client.refresh_sdk_catalogs_acknowledged();
        let native = async {
            written.recv().await.unwrap().ack.send(Ok(())).unwrap();
        };
        let (result, ()) = tokio::join!(refresh, native);
        assert!(matches!(result, Err(Error::Uncertain)));
        assert!(client.shared.lock().pending.is_empty());
        assert!(written.try_recv().is_err());
    }

    #[tokio::test]
    async fn refresh_rejection_and_cancellation_retire_only_the_waiter() {
        let (client, mut written) = handshake_transport();
        client.shared.initialized.store(2, Ordering::Release);
        let rejecting = client.refresh_sdk_catalogs_acknowledged();
        let native = async {
            let write = written.recv().await.unwrap();
            let notification: Value = serde_json::from_slice(&write.frame).unwrap();
            write.ack.send(Ok(())).unwrap();
            handle_control_response(
                &json!({"response":{"subtype":"error",
                "request_id":notification["request_id"],"error":"secret native error"}}),
                &client.shared,
                &mut client.shared.lock(),
            )
            .unwrap();
        };
        let (result, ()) = tokio::join!(rejecting, native);
        assert!(matches!(result, Err(Error::Protocol)));
        assert!(client.shared.lock().pending.is_empty());
        let mut cancelling = Box::pin(client.refresh_sdk_catalogs_acknowledged());
        tokio::select! {
            result = &mut cancelling => panic!("unexpected completion: {result:?}"),
            write = written.recv() => {
                let write = write.unwrap();
                assert_eq!(client.shared.lock().pending.len(), 1);
                write.ack.send(Ok(())).unwrap();
            }
        }
        drop(cancelling);
        assert!(client.shared.lock().pending.is_empty());
        assert!(written.try_recv().is_err());
    }

    #[tokio::test]
    async fn refresh_retains_correlation_and_routes_catalog_after_startup() {
        let (client, mut written) = handshake_transport();
        let (events, mut received) = event_channel(1 << 20);
        route(
            json!({"type":"control_request","request_id":"startup-list",
            "request":{"subtype":"mcp_message","server_name":"bcode-tools",
            "message":{"jsonrpc":"2.0","id":1,"method":"tools/list"}}}),
            &client.shared,
            &events,
        )
        .unwrap();
        let startup = written.recv().await.unwrap();
        let payload: Value = serde_json::from_slice(&startup.frame).unwrap();
        assert_eq!(
            payload["response"]["response"]["mcp_response"]["result"]["tools"],
            json!([])
        );
        assert!(received.try_recv().is_err());
        client.shared.initialized.store(2, Ordering::Release);
        let refreshing = client.refresh_sdk_catalogs_acknowledged();
        let native = async {
            let write = written.recv().await.unwrap();
            let notification: Value = serde_json::from_slice(&write.frame).unwrap();
            assert_eq!(
                notification["request"]["message"]["method"],
                "notifications/tools/list_changed"
            );
            let id = notification["request_id"].as_str().unwrap();
            assert!(client.shared.lock().pending.contains_key(id));
            write.ack.send(Ok(())).unwrap();
            route(
                json!({"type":"control_request","request_id":"current-list",
                "request":{"subtype":"mcp_message","server_name":"bcode-tools",
                "message":{"jsonrpc":"2.0","id":2,"method":"tools/list"}}}),
                &client.shared,
                &events,
            )
            .unwrap();
            assert!(matches!(
                received.recv().await.unwrap(),
                Event::Callback { .. }
            ));
            route(
                json!({"type":"control_response","response":{"subtype":"success",
                "request_id":id,"response":{}}}),
                &client.shared,
                &events,
            )
            .unwrap();
        };
        let (result, ()) = tokio::join!(refreshing, native);
        result.unwrap();
        assert!(client.shared.lock().pending.is_empty());
        assert!(client.shared.lock().callbacks.contains_key("current-list"));
        assert_eq!(client.sdk_metadata_counts(), [0, 1, 1]);
    }

    #[test]
    fn bound_catalog_notification_never_waits_for_native_or_callback_pump() {
        let (client, mut written) = handshake_transport();
        client.shared.initialized.store(2, Ordering::Release);
        client.refresh_sdk_catalogs().unwrap();
        // No writer task, native process or acknowledgement exists. Enqueue must suffice.
        let notification: Value =
            serde_json::from_slice(&written.try_recv().unwrap().frame).unwrap();
        assert_eq!(notification["request"]["subtype"], "mcp_message");
        assert_eq!(notification["request"]["server_name"], "bcode-tools");
        assert_eq!(
            notification["request"]["message"],
            json!({
                "jsonrpc":"2.0", "method":"notifications/tools/list_changed",
            })
        );
        // Neither canonical effects nor a pending control waiter are created.
        assert!(client.shared.lock().pending.is_empty());
        assert!(client.shared.lock().callbacks.is_empty());
        for _ in 0..8 {
            client.refresh_sdk_catalogs().unwrap();
        }
        assert!(matches!(
            client.refresh_sdk_catalogs(),
            Err(Error::Capacity)
        ));
        drop(client);
    }

    #[test]
    fn startup_negotiation_needs_no_plugin_consumer_or_tool_grant() {
        let (client, mut written) = handshake_transport();
        let shared = &client.shared;
        let (events, mut received) = event_channel(1 << 20);
        for (id, method) in [
            (Some(json!(0)), "initialize"),
            (None, "notifications/initialized"),
            (Some(json!(1)), "tools/list"),
        ] {
            let mut message = json!({"jsonrpc":"2.0","method":method});
            if let Some(id) = id {
                message["id"] = id;
            }
            route(
                json!({"type":"control_request","request_id":"sdk-handshake",
                "request":{"subtype":"mcp_message","server_name":"bcode-tools","message":message}}),
                shared,
                &events,
            )
            .unwrap();
            let response: Value =
                serde_json::from_slice(&written.try_recv().unwrap().frame).unwrap();
            assert_eq!(response["response"]["request_id"], "sdk-handshake");
            if method == "initialize" {
                assert_eq!(
                    response["response"]["response"]["mcp_response"]["result"]["capabilities"],
                    json!({"tools":{"listChanged":true}})
                );
            }
            assert!(
                response["response"]["response"]["mcp_response"]
                    .get("error")
                    .is_none()
            );
        }
        assert!(received.try_recv().is_err());
        assert!(shared.lock().callbacks.is_empty());
        shared.initialized.store(2, Ordering::Release);
        route(
            json!({"type":"control_request","request_id":"catalog-after-init",
            "request":{"subtype":"mcp_message","server_name":"bcode-tools",
                "message":{"jsonrpc":"2.0","id":2,"method":"tools/list"}}}),
            shared,
            &events,
        )
        .unwrap();
        assert!(matches!(
            received.try_recv().unwrap(),
            Event::Callback { .. }
        ));
        assert!(written.try_recv().is_err());
        assert!(
            client.can_submit_next_turn(),
            "metadata refresh is not foreground tool execution"
        );
        client.shared.lock().callbacks.insert("actual-call".into(), CallbackRequest::McpMessage {
            server_name: "bcode-tools".into(),
            message: json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"filesystem.read","arguments":{}}}),
        });
        assert!(
            !client.can_submit_next_turn(),
            "actual tool callbacks still block new input"
        );
        drop(client);
    }
}

#[cfg(test)]
mod retired_delivery_tests {
    use super::*;

    #[test]
    fn retired_tool_frame_never_reaches_message_consumers_or_later_task_origin() {
        const A: &str = "00000000-0000-4000-8000-000000000001";
        const B: &str = "00000000-0000-4000-8000-000000000002";
        const FRAME: &str = "11111111-1111-4111-8111-000000000001";
        for frame_id in [FRAME, A] {
            let mut attribution = crate::attribution::AttributionLedger::new();
            let a = crate::SubmissionUuid::new(A.into()).unwrap();
            attribution.retain(a.clone()).unwrap();
            attribution
                .observe(&json!({"type":"assistant","uuid":FRAME,"user_message_uuid":A}))
                .unwrap();
            attribution.retire(&a);
            attribution
                .retain(crate::SubmissionUuid::new(B.into()).unwrap())
                .unwrap();
            let (writes, _writer) = mpsc::channel(8);
            let (stop, _stopped) = watch::channel(false);
            let shared = Shared {
                model_selection_contract: crate::models::ModelSelectionContract::ExactIdentity,
                native_version: "test-only".into(),
                reaped: watch::channel(ReapState::Pending).0,
                state: Mutex::new(State {
                    pending: BTreeMap::new(),
                    callbacks: BTreeMap::new(),
                    responding: BTreeSet::new(),
                    lifecycle: Lifecycle::new(crate::SettlementSupport::VerifiedSessionState, 8),
                    attribution,
                    attributed_mode: Some(true),
                    fault: None,
                    expected_session_id: None,
                    confirmed_session_id: None,
                    initialization_request_id: None,
                    skipped_tools: BTreeSet::new(),
                }),
                stop,
                writes,
                counter: AtomicU64::new(1),
                initialized: AtomicU8::new(0),
                identity_changed: Notify::new(),
                limits: crate::Limits::default(),
                sdk_servers: BTreeSet::new(),
                sdk_metadata: std::array::from_fn(|_| AtomicU64::new(0)),
            };
            let (events, mut received) = event_channel(1 << 20);
            route(
                json!({"type":"assistant","uuid":frame_id,"user_message_uuid":A,
            "message":{"content":[{"type":"tool_use","id":"late-tool","name":"Bash"}]}}),
                &shared,
                &events,
            )
            .unwrap();
            // Its task frames are skipped too, so neither can be associated with B; the
            // session itself continues.
            route(json!({"type":"system","subtype":"task_started","task_id":"late-task","tool_use_id":"late-tool"}), &shared, &events).unwrap();
            for _ in 0..2 {
                let Ok(Event::Message(NativeMessage::Notice(notice))) = received.try_recv() else {
                    panic!("only skipped-frame notices reach consumers");
                };
                assert_eq!(notice["code"], "uncorrelated_frame");
            }
            assert!(received.try_recv().is_err());
            assert!(shared.lock().fault.is_none());
        }
    }
}

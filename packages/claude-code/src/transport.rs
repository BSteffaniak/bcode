use crate::{
    CallbackRequest, CallbackResponse, ControlRequest, Error, Event, LaunchOptions, NativeMessage,
    Settlement, protocol::Lifecycle,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    sync::{Notify, mpsc, oneshot, watch},
    task::JoinHandle,
};

type Pending = oneshot::Sender<Result<Value, Error>>;
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
struct Shared {
    state: Mutex<State>,
    stop: watch::Sender<bool>,
    writes: mpsc::Sender<Write>,
    counter: AtomicU64,
    initialized: AtomicU8,
    identity_changed: Notify,
    limits: crate::Limits,
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
/// Cloneable control handle. All clones address only the originally spawned process.
#[derive(Clone)]
pub struct ClaudeClient {
    shared: Arc<Shared>,
}
/// Single-consumer observation stream and process lifetime owner.
/// Dropping kills/reaps the process asynchronously and invalidates callbacks.
pub struct ClaudeSession {
    client: ClaudeClient,
    events: mpsc::Receiver<Event>,
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
        let auth = crate::auth_status(&options).await?;
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
        let (event_tx, events) = mpsc::channel(options.limits.queued_events);
        let (write_tx, write_rx) = mpsc::channel(options.limits.queued_writes);
        let (stop, mut stopped) = watch::channel(false);
        let shared = Arc::new(Shared {
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
            }),
            stop,
            writes: write_tx,
            counter: AtomicU64::new(1),
            initialized: AtomicU8::new(0),
            identity_changed: Notify::new(),
            limits: options.limits,
        });
        let reader_shared = Arc::clone(&shared);
        let mut reader = tokio::spawn(async move {
            read_stdout(stdout, reader_shared, event_tx).await;
        });
        // Stderr is intentionally drained without retaining secret-bearing diagnostics.
        let err_shared = Arc::clone(&shared);
        let mut stderr_reader = tokio::spawn(async move {
            if drain(stderr).await.is_err() {
                err_shared.fail(Fault::Closed);
            }
        });
        let writer_shared = Arc::clone(&shared);
        let writer = tokio::spawn(async move {
            write_stdin(stdin, write_rx, writer_shared).await;
        });
        let supervisor_shared = Arc::clone(&shared);
        let supervisor = tokio::spawn(async move {
            tokio::select! {
                _ = child.wait() => {},
                _ = stopped.changed() => { let _ = child.kill().await; let _ = child.wait().await; },
            }
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
        });
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
    /// Kill and reap the process. Cancellation is never reported as proof of no effects.
    pub async fn close(mut self) {
        self.client.terminate();
        if let Some(supervisor) = self.supervisor.take() {
            let _ = supervisor.await;
        }
    }
}
impl Drop for ClaudeSession {
    fn drop(&mut self) {
        self.client.terminate();
    }
}
impl ClaudeClient {
    #[must_use]
    pub fn settlement(&self) -> Settlement {
        self.shared.lock().settlement()
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
        let native_request: Value =
            serde_json::from_slice(&bound.bytes).map_err(|_| Error::Protocol)?;
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
    /// write does not release it or authorize retry. At most 256 UUIDs are retained
    /// for this process lifetime; callers must not mix attributed and legacy sends.
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
            if !state.lifecycle.can_submit() || !state.callbacks.is_empty() {
                return Err(Error::InvalidConfiguration(
                    "active turn steering is unsupported; wait for authoritative settlement",
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
    events: mpsc::Sender<Event>,
) {
    let mut chunk = [0_u8; 8192];
    let mut frame = Vec::new();
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
                if frame.is_empty() {
                    continue;
                }
                let result = crate::attribution::decode_frame(&frame)
                    .map_err(|_| Error::Protocol)
                    .and_then(|value| route(value, &shared, &events));
                frame.clear();
                if let Err(error) = result {
                    shared.fail(match error {
                        Error::Capacity => Fault::Capacity,
                        Error::SessionIdentity => Fault::SessionIdentity,
                        _ => Fault::Protocol,
                    });
                    failed = true;
                }
            } else if frame.len() >= shared.limits.frame_bytes {
                shared.fail(Fault::Capacity);
                frame.clear();
                failed = true;
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
fn send_event(events: &mpsc::Sender<Event>, event: Event) -> Result<(), Error> {
    events.try_send(event).map_err(|_| Error::Capacity)
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
fn handle_control_request(
    value: &Value,
    shared: &Shared,
    state: &mut State,
    events: &mpsc::Sender<Event>,
) -> Result<(), Error> {
    let id = value
        .get("request_id")
        .and_then(Value::as_str)
        .ok_or(Error::Protocol)?;
    crate::launch::validate_id(id)?;
    if state.callbacks.contains_key(id) {
        return Err(Error::Protocol);
    }
    if state.callbacks.len() >= shared.limits.active_callbacks {
        return Err(Error::Capacity);
    }
    let request: CallbackRequest =
        serde_json::from_value(value.get("request").cloned().ok_or(Error::Protocol)?)
            .map_err(|_| Error::Protocol)?;
    state.lifecycle.attention();
    state.callbacks.insert(id.to_owned(), request.clone());
    send_event(
        events,
        Event::Callback {
            request_id: id.to_owned(),
            request,
        },
    )
}
fn route(value: Value, shared: &Shared, events: &mpsc::Sender<Event>) -> Result<(), Error> {
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or(Error::Protocol)?;
    let mut state = shared.lock();
    if state.fault.is_some() {
        return Ok(());
    }
    match kind {
        "control_response" => {
            handle_control_response(&value, shared, &mut state)?;
        }
        "control_request" => {
            handle_control_request(&value, shared, &mut state, events)?;
        }
        "control_cancel_request" => {
            let id = value
                .get("request_id")
                .and_then(Value::as_str)
                .ok_or(Error::Protocol)?;
            state.responding.remove(id);
            if state.callbacks.remove(id).is_some() {
                send_event(
                    events,
                    Event::CallbackCancelled {
                        request_id: id.to_owned(),
                    },
                )?;
            }
        }
        "system" | "assistant" | "user" | "result" | "stream_event" | "command_lifecycle"
        | "rate_limit_event" => {
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
            let attribution = state.attribution.observe(&value)?;
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
            if kind != "result"
                || state.attributed_mode != Some(true)
                || state.attribution.answers_current(attribution.as_ref())
            {
                state.lifecycle.observe(&value)?;
            }
            let next = state.settlement();
            drop(state);
            let message = match kind {
                "system" => NativeMessage::System(value),
                "assistant" => NativeMessage::Assistant(value),
                "user" => NativeMessage::User(value),
                "result" => NativeMessage::Result(value),
                "stream_event" => NativeMessage::Stream(value),
                _ => NativeMessage::Notice(value),
            };
            if let Some(attribution) = attribution {
                send_event(events, Event::Attribution(attribution))?;
            }
            send_event(events, Event::Message(message))?;
            if next != previous {
                send_event(events, Event::Settlement(next))?;
            }
        }
        _ => return Err(Error::Protocol),
    }
    Ok(())
}

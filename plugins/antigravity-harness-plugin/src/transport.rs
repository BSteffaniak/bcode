//! Bounded JSON-RPC 2.0 stdio transport for `agy acp --stdio --disable-telemetry`.
//!
//! * Dedicated stdout and stderr reader threads drain pipes continuously.
//! * A supervisor thread owns the child process and sets `reaped` only after physical `wait()`.
//! * Inbound notifications, inbound server->client requests (e.g. `session/request_permission`),
//!   and RPC replies are bounded; queue overflow fences the entire transport rather than silently
//!   dropping a permission callback.
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use thiserror::Error;
use zeroize::Zeroize;

/// Maximum UTF-8 bytes accepted for one JSON-RPC line from stdout.
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
/// Maximum queued inbound events (notifications + server requests) before overflow fencing.
pub const MAX_EVENT_QUEUE: usize = 512;
/// Maximum total bytes of queued inbound events before overflow fencing.
pub const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;
/// Maximum stderr bytes retained (drained continuously, never blocks child).
const MAX_STDERR_BYTES: usize = 64 * 1024;
/// Non-secret host environment variables forwarded to the child process.
const ALLOWED_ENV: [&str; 7] = ["HOME", "USER", "PATH", "TMPDIR", "LANG", "LC_ALL", "TERM"];

/// Transport and adapter error categories; never carry raw native diagnostics or secrets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum AcpError {
    #[error("stale owner or generation")]
    Stale,
    #[error("unsupported native capability or request")]
    Unsupported,
    #[error("invalid or mismatched adapter configuration")]
    Configuration,
    #[error("payload or queue bounds exceeded")]
    Bounds,
    #[error("malformed or unexpected native protocol frame")]
    Protocol,
    #[error("native rejected the requested operation")]
    Rejected,
    #[error("native process disconnected")]
    Disconnected,
    #[error("native delivery or state is uncertain")]
    Uncertain,
}

/// Launch specification for one `agy` process.
#[derive(Debug, Clone)]
pub struct Launch {
    pub executable: PathBuf,
    pub gemini_dir: PathBuf,
    pub working_directory: PathBuf,
    pub timeout: Duration,
}

impl Launch {
    /// Validate absolute paths before spawning.
    ///
    /// # Errors
    /// * Rejects relative paths or missing directories.
    pub fn validate(&self) -> Result<(), AcpError> {
        if !self.executable.is_absolute()
            || !self.gemini_dir.is_absolute()
            || !self.working_directory.is_absolute()
        {
            return Err(AcpError::Configuration);
        }
        Ok(())
    }

    fn base_command(&self) -> Result<Command, AcpError> {
        self.validate()?;
        let mut cmd = Command::new(&self.executable);
        cmd.current_dir(&self.working_directory);
        cmd.env_clear();
        for key in ALLOWED_ENV {
            if let Ok(value) = std::env::var(key) {
                cmd.env(key, value);
            }
        }
        Ok(cmd)
    }
}

/// Probe `agy --version` with a bounded timeout and sanitized environment.
///
/// # Errors
/// * Returns `Disconnected` if the executable fails to run or exits non-zero,
///   or `Protocol` if stdout is empty or malformed.
pub fn probe_version(launch: &Launch) -> Result<String, AcpError> {
    let output = run_bounded_cli(launch, &["--version"])?;
    let version = output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .ok_or(AcpError::Protocol)?;
    if version.len() > bcode_harness_models::MAX_HARNESS_ID_BYTES
        || version.chars().any(char::is_control)
    {
        return Err(AcpError::Protocol);
    }
    Ok(version.to_owned())
}

/// Run a short-lived `agy` subcommand (such as `--version` or `models`) with bounded stdout/stderr
/// and a hard timeout.
///
/// # Errors
/// * Returns `Disconnected` on spawn/timeout/non-zero exit, or `Bounds` if output is oversized.
pub fn run_bounded_cli(launch: &Launch, args: &[&str]) -> Result<String, AcpError> {
    let mut cmd = launch.base_command()?;
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|_| AcpError::Disconnected)?;
    let stdout = child.stdout.take().ok_or(AcpError::Disconnected)?;
    let stderr = child.stderr.take().ok_or(AcpError::Disconnected)?;

    let out_handle = std::thread::spawn(move || read_bounded_bytes(stdout, 256 * 1024));
    let err_handle = std::thread::spawn(move || read_bounded_bytes(stderr, MAX_STDERR_BYTES));

    let deadline = Instant::now() + launch.timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = out_handle.join();
                    let _ = err_handle.join();
                    return Err(AcpError::Disconnected);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = out_handle.join();
                let _ = err_handle.join();
                return Err(AcpError::Disconnected);
            }
        }
    };
    let out_bytes = out_handle
        .join()
        .map_err(|_| AcpError::Disconnected)?
        .map_err(|()| AcpError::Bounds)?;
    let _ = err_handle.join();
    if !status.success() {
        return Err(AcpError::Disconnected);
    }
    String::from_utf8(out_bytes).map_err(|_| AcpError::Protocol)
}

fn read_bounded_bytes<R: Read>(reader: R, limit: usize) -> Result<Vec<u8>, ()> {
    let mut buf = Vec::new();
    let mut limited = reader.take((limit as u64) + 1);
    if limited.read_to_end(&mut buf).is_err() {
        return Err(());
    }
    if buf.len() > limit {
        return Err(());
    }
    Ok(buf)
}

/// Condition-variable wake handle shared with waiting `observe` invocations.
#[derive(Clone)]
pub struct Wake {
    inner: Arc<(Mutex<u64>, Condvar)>,
}

impl Default for Wake {
    fn default() -> Self {
        Self {
            inner: Arc::new((Mutex::new(0), Condvar::new())),
        }
    }
}

impl Wake {
    /// Current monotonic wake generation.
    #[must_use]
    pub fn seen(&self) -> u64 {
        self.inner.0.lock().map_or(0, |guard| *guard)
    }

    /// Increment generation and wake all waiters.
    pub fn notify(&self) {
        if let Ok(mut guard) = self.inner.0.lock() {
            *guard = guard.saturating_add(1);
            self.inner.1.notify_all();
        }
    }

    /// Wait up to `timeout` until generation differs from `seen`.
    #[must_use]
    pub fn wait_changed(&self, seen: u64, timeout: Duration) -> bool {
        let Ok(guard) = self.inner.0.lock() else {
            return true;
        };
        if *guard != seen {
            return true;
        }
        let Ok((guard, _)) = self.inner.1.wait_timeout(guard, timeout) else {
            return true;
        };
        *guard != seen
    }
}

/// Readiness of the transport queue for a waiting `observe`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    /// Durable events, callbacks, replies, or disconnect are ready now.
    Ready,
    /// Only streaming text/thought chunks are queued.
    DeltasOnly,
    /// Queue is empty and transport is healthy.
    Idle,
}

/// Inbound frame classified by the reader thread.
#[derive(Debug)]
pub enum InboundEvent {
    /// Server -> client JSON-RPC notification (`session/update`, etc.).
    Notification { method: String, params: Value },
    /// Server -> client JSON-RPC request (`session/request_permission`).
    ServerRequest {
        id: Value,
        method: String,
        params: Value,
    },
    /// Completed asynchronous RPC reply (used for `session/prompt`).
    AsyncReply { id: u64, message: Value },
    /// Non-fatal frame error (oversized or unparseable line); reported once as a notice.
    MalformedFrame(&'static str),
}

struct SharedState {
    events: VecDeque<(InboundEvent, usize)>,
    event_bytes: usize,
    pending_sync: BTreeMap<u64, Option<Value>>,
    async_ids: std::collections::BTreeSet<u64>,
    closed: bool,
    overflowed: bool,
}

impl SharedState {
    const fn new() -> Self {
        Self {
            events: VecDeque::new(),
            event_bytes: 0,
            pending_sync: BTreeMap::new(),
            async_ids: std::collections::BTreeSet::new(),
            closed: false,
            overflowed: false,
        }
    }

    fn push_event(&mut self, event: InboundEvent, bytes: usize) -> bool {
        if self.closed || self.overflowed {
            return false;
        }
        if self.events.len() >= MAX_EVENT_QUEUE
            || self.event_bytes.saturating_add(bytes) > MAX_EVENT_BYTES
        {
            self.overflowed = true;
            self.closed = true;
            return false;
        }
        self.event_bytes = self.event_bytes.saturating_add(bytes);
        self.events.push_back((event, bytes));
        true
    }
}

/// Running `agy acp --stdio --disable-telemetry` process with bounded framing and reaping.
pub struct AcpServer {
    stdin: Option<std::process::ChildStdin>,
    shared: Arc<(Mutex<SharedState>, Condvar)>,
    wake: Wake,
    next_id: u64,
    timeout: Duration,
    reaped: Arc<AtomicBool>,
    kill_tx: Option<std::sync::mpsc::Sender<()>>,
    native_version: Option<String>,
}

impl AcpServer {
    /// Spawn a fresh `agy acp --stdio --disable-telemetry` child process.
    ///
    /// # Errors
    /// * Returns `Configuration` for invalid paths or `Disconnected` if spawning fails.
    pub fn spawn(launch: &Launch) -> Result<Self, AcpError> {
        let mut cmd = launch.base_command()?;
        cmd.args(["acp", "--stdio", "--disable-telemetry", "--gemini-dir"])
            .arg(&launch.gemini_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(|_| AcpError::Disconnected)?;
        let stdin = child.stdin.take().ok_or(AcpError::Disconnected)?;
        let stdout = child.stdout.take().ok_or(AcpError::Disconnected)?;
        let stderr = child.stderr.take().ok_or(AcpError::Disconnected)?;

        let shared = Arc::new((Mutex::new(SharedState::new()), Condvar::new()));
        let wake = Wake::default();
        let reaped = Arc::new(AtomicBool::new(false));
        let (kill_tx, kill_rx) = std::sync::mpsc::channel::<()>();

        // Supervisor thread owns child reaping.
        let sup_reaped = Arc::clone(&reaped);
        let sup_shared = Arc::clone(&shared);
        let sup_wake = wake.clone();
        std::thread::spawn(move || {
            supervise_child(child, &kill_rx, &sup_reaped, &sup_shared, &sup_wake);
        });

        // Continuous stderr drainer so child never deadlocks on stderr pipe.
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut discard = [0u8; 4096];
            while let Ok(n) = reader.read(&mut discard) {
                if n == 0 {
                    break;
                }
            }
        });

        // Continuous stdout JSON-RPC reader.
        let reader_shared = Arc::clone(&shared);
        let reader_wake = wake.clone();
        let reader_kill = kill_tx.clone();
        std::thread::spawn(move || {
            read_stdout(stdout, &reader_shared, &reader_wake, &reader_kill);
        });

        Ok(Self {
            stdin: Some(stdin),
            shared,
            wake,
            next_id: 1,
            timeout: launch.timeout,
            reaped,
            kill_tx: Some(kill_tx),
            native_version: None,
        })
    }

    /// Perform the ACP `initialize` handshake and return the reported server version.
    ///
    /// # Errors
    /// * Returns `Protocol` if `protocolVersion` or `agentInfo.version` is missing/invalid.
    pub fn initialize(&mut self, _launch: &Launch) -> Result<String, AcpError> {
        let result = self.call(
            "initialize",
            &json!({
                "protocolVersion": 1,
                "clientCapabilities": {
                    "fs": {
                        "readTextFile": false,
                        "writeTextFile": false
                    },
                    "terminal": false
                }
            }),
        )?;
        let version = result
            .get("agentInfo")
            .and_then(|info| info.get("version"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty() && v.len() <= bcode_harness_models::MAX_HARNESS_ID_BYTES)
            .ok_or(AcpError::Protocol)?
            .to_owned();
        self.native_version = Some(version.clone());
        Ok(version)
    }

    /// Return the native version reported during `initialize`, if any.
    #[must_use]
    pub fn native_version(&self) -> Option<&str> {
        self.native_version.as_deref()
    }

    /// Return the supervisor's physical reap flag.
    #[must_use]
    pub fn reap_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.reaped)
    }

    /// Return the wake handle for waiting `observe` calls.
    #[must_use]
    pub fn wake(&self) -> Wake {
        self.wake.clone()
    }

    /// Whether the transport is closed or overflowed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.shared
            .0
            .lock()
            .map_or(true, |state| state.closed || state.overflowed)
    }

    /// Whether the transport fenced due to inbound queue overflow.
    #[must_use]
    pub fn is_overflowed(&self) -> bool {
        self.shared.0.lock().is_ok_and(|state| state.overflowed)
    }

    /// Inspect current queue readiness without draining.
    #[must_use]
    pub fn readiness(&self) -> Readiness {
        let Ok(state) = self.shared.0.lock() else {
            return Readiness::Ready;
        };
        if state.closed || state.overflowed {
            return Readiness::Ready;
        }
        if state.events.is_empty() {
            return Readiness::Idle;
        }
        let all_deltas = state.events.iter().all(|(event, _)| is_live_delta(event));
        if all_deltas {
            Readiness::DeltasOnly
        } else {
            Readiness::Ready
        }
    }

    /// Drain up to `limit` queued inbound events.
    pub fn drain_events(&mut self, limit: usize) -> Vec<InboundEvent> {
        let Ok(mut state) = self.shared.0.lock() else {
            return Vec::new();
        };
        let count = limit.min(state.events.len());
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            if let Some((event, bytes)) = state.events.pop_front() {
                state.event_bytes = state.event_bytes.saturating_sub(bytes);
                out.push(event);
            }
        }
        out
    }

    /// Send a synchronous JSON-RPC request and wait for its `result` (mapping `error` to `Rejected`).
    ///
    /// # Errors
    /// * Returns `Rejected` on JSON-RPC error, `Disconnected` on closed pipe, `Uncertain` on timeout.
    pub fn call(&mut self, method: &str, params: &Value) -> Result<Value, AcpError> {
        let msg = self.call_raw(method, params)?;
        if msg.get("error").is_some() {
            return Err(AcpError::Rejected);
        }
        msg.get("result").cloned().ok_or(AcpError::Protocol)
    }

    /// Send a synchronous JSON-RPC request and return the full JSON-RPC reply object.
    ///
    /// # Errors
    /// * Returns `Disconnected` on closed pipe or `Uncertain` on timeout/overflow.
    pub fn call_raw(&mut self, method: &str, params: &Value) -> Result<Value, AcpError> {
        let id = self.next_rpc_id()?;
        {
            let mut state = self.shared.0.lock().map_err(|_| AcpError::Uncertain)?;
            if state.closed || state.overflowed {
                return Err(AcpError::Disconnected);
            }
            state.pending_sync.insert(id, None);
        }
        if let Err(err) = self.write_json(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        })) {
            if let Ok(mut state) = self.shared.0.lock() {
                state.pending_sync.remove(&id);
            }
            return Err(err);
        }
        let deadline = Instant::now() + self.timeout;
        let mut state = self.shared.0.lock().map_err(|_| AcpError::Uncertain)?;
        loop {
            if let Some(Some(reply)) = state.pending_sync.get_mut(&id).map(Option::take) {
                state.pending_sync.remove(&id);
                return Ok(reply);
            }
            if state.overflowed {
                state.pending_sync.remove(&id);
                return Err(AcpError::Uncertain);
            }
            if state.closed {
                state.pending_sync.remove(&id);
                return Err(AcpError::Disconnected);
            }
            let now = Instant::now();
            if now >= deadline {
                state.pending_sync.remove(&id);
                state.closed = true;
                self.request_kill();
                return Err(AcpError::Uncertain);
            }
            let wait = (deadline - now).min(Duration::from_millis(100));
            let Ok((next_state, _)) = self.shared.1.wait_timeout(state, wait) else {
                return Err(AcpError::Uncertain);
            };
            state = next_state;
        }
    }

    /// Send an asynchronous JSON-RPC request (used for `session/prompt`); its reply arrives later
    /// as `InboundEvent::AsyncReply { id, message }`.
    ///
    /// # Errors
    /// * Returns `Disconnected` if the transport is closed or writing fails.
    pub fn send_async_request(&mut self, method: &str, params: &Value) -> Result<u64, AcpError> {
        let id = self.next_rpc_id()?;
        {
            let mut state = self.shared.0.lock().map_err(|_| AcpError::Uncertain)?;
            if state.closed || state.overflowed {
                return Err(AcpError::Disconnected);
            }
            state.async_ids.insert(id);
        }
        if let Err(err) = self.write_json(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        })) {
            if let Ok(mut state) = self.shared.0.lock() {
                state.async_ids.remove(&id);
            }
            return Err(err);
        }
        Ok(id)
    }

    /// Send a JSON-RPC notification (no `id`, no reply expected).
    ///
    /// # Errors
    /// * Returns `Disconnected` if the transport is closed or writing fails.
    pub fn notify(&mut self, method: &str, params: &Value) -> Result<(), AcpError> {
        self.write_json(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
    }

    /// Send a JSON-RPC reply to an inbound server->client request (e.g. `session/request_permission`).
    ///
    /// # Errors
    /// * Returns `Disconnected` if the transport is closed or writing fails.
    pub fn reply_result(&mut self, id: &Value, result: &Value) -> Result<(), AcpError> {
        self.write_json(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }))
    }

    /// Send a JSON-RPC error reply to an unsupported inbound server->client request.
    ///
    /// # Errors
    /// * Returns `Disconnected` if the transport is closed or writing fails.
    pub fn reply_error(&mut self, id: &Value, code: i64, message: &str) -> Result<(), AcpError> {
        self.write_json(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": code,
                "message": message,
            },
        }))
    }

    /// Gracefully close stdin and ask the supervisor to terminate and reap the child.
    pub fn close(&mut self) {
        self.stdin.take();
        if let Ok(mut state) = self.shared.0.lock() {
            state.closed = true;
            self.shared.1.notify_all();
        }
        self.wake.notify();
        self.request_kill();
    }

    fn next_rpc_id(&mut self) -> Result<u64, AcpError> {
        let id = self.next_id;
        self.next_id = self.next_id.checked_add(1).ok_or(AcpError::Bounds)?;
        Ok(id)
    }

    fn write_json(&mut self, payload: &Value) -> Result<(), AcpError> {
        let stdin = self.stdin.as_mut().ok_or(AcpError::Disconnected)?;
        let encoded = serde_json::to_string(payload).map_err(|_| AcpError::Bounds)?;
        if encoded.len() > MAX_FRAME_BYTES {
            return Err(AcpError::Bounds);
        }
        stdin
            .write_all(encoded.as_bytes())
            .and_then(|()| stdin.write_all(b"\n"))
            .and_then(|()| stdin.flush())
            .map_err(|_| AcpError::Disconnected)
    }

    fn request_kill(&self) {
        if let Some(tx) = &self.kill_tx {
            let _ = tx.send(());
        }
    }
}

impl Drop for AcpServer {
    fn drop(&mut self) {
        self.close();
    }
}

fn is_live_delta(event: &InboundEvent) -> bool {
    let InboundEvent::Notification { method, params } = event else {
        return false;
    };
    if method != "session/update" {
        return false;
    }
    matches!(
        params
            .get("update")
            .and_then(|u| u.get("sessionUpdate"))
            .and_then(Value::as_str),
        Some("agent_message_chunk" | "agent_thought_chunk")
    )
}

fn read_stdout(
    stdout: std::process::ChildStdout,
    shared: &Arc<(Mutex<SharedState>, Condvar)>,
    wake: &Wake,
    kill_tx: &std::sync::mpsc::Sender<()>,
) {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    loop {
        line.clear();
        let Ok(n) = read_bounded_line(&mut reader, &mut line, MAX_FRAME_BYTES) else {
            break;
        };
        if n == 0 {
            break;
        }
        if n > MAX_FRAME_BYTES {
            push_or_fence(
                shared,
                wake,
                kill_tx,
                InboundEvent::MalformedFrame("oversized_frame"),
                64,
            );
            continue;
        }
        let Ok(text) = std::str::from_utf8(&line) else {
            push_or_fence(
                shared,
                wake,
                kill_tx,
                InboundEvent::MalformedFrame("invalid_utf8_frame"),
                64,
            );
            continue;
        };
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
            push_or_fence(
                shared,
                wake,
                kill_tx,
                InboundEvent::MalformedFrame("unparseable_frame"),
                64,
            );
            continue;
        };
        let frame_bytes = trimmed.len();
        dispatch_inbound(msg, frame_bytes, shared, wake, kill_tx);
    }
    if let Ok(mut state) = shared.0.lock() {
        state.closed = true;
        shared.1.notify_all();
    }
    wake.notify();
}

fn dispatch_inbound(
    msg: Value,
    frame_bytes: usize,
    shared: &Arc<(Mutex<SharedState>, Condvar)>,
    wake: &Wake,
    kill_tx: &std::sync::mpsc::Sender<()>,
) {
    let has_method = msg.get("method").and_then(Value::as_str).is_some();
    let has_id = msg.get("id").is_some_and(|v| !v.is_null());

    if has_method && has_id {
        // Server -> client JSON-RPC request (e.g. session/request_permission)
        let method = msg["method"].as_str().unwrap_or_default().to_owned();
        let id = msg["id"].clone();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        push_or_fence(
            shared,
            wake,
            kill_tx,
            InboundEvent::ServerRequest { id, method, params },
            frame_bytes,
        );
        return;
    }

    if has_method {
        // Notification
        let method = msg["method"].as_str().unwrap_or_default().to_owned();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        push_or_fence(
            shared,
            wake,
            kill_tx,
            InboundEvent::Notification { method, params },
            frame_bytes,
        );
        return;
    }

    if let Some(id) = msg.get("id").and_then(Value::as_u64) {
        let mut overflow = false;
        if let Ok(mut state) = shared.0.lock() {
            if let Some(slot) = state.pending_sync.get_mut(&id) {
                *slot = Some(msg);
                shared.1.notify_all();
                return;
            }
            if state.async_ids.remove(&id) {
                overflow =
                    !state.push_event(InboundEvent::AsyncReply { id, message: msg }, frame_bytes);
                shared.1.notify_all();
            }
        }
        if overflow {
            let _ = kill_tx.send(());
        }
        wake.notify();
    }
}

fn push_or_fence(
    shared: &Arc<(Mutex<SharedState>, Condvar)>,
    wake: &Wake,
    kill_tx: &std::sync::mpsc::Sender<()>,
    event: InboundEvent,
    bytes: usize,
) {
    let overflow = shared.0.lock().map_or(true, |mut state| {
        let ok = state.push_event(event, bytes);
        shared.1.notify_all();
        !ok
    });
    if overflow {
        let _ = kill_tx.send(());
    }
    wake.notify();
}

/// Read up to `max_bytes` until `\n`. If a line exceeds `max_bytes`, drain the remainder of that
/// line without storing it and return `max_bytes + 1` so the caller can emit a bounded notice.
fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max_bytes: usize,
) -> std::io::Result<usize> {
    let mut total = 0usize;
    let mut oversized = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(if oversized { max_bytes + 1 } else { total });
        }
        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            let take = pos + 1;
            if !oversized && total + take <= max_bytes {
                buf.extend_from_slice(&available[..take]);
            } else {
                oversized = true;
            }
            total = total.saturating_add(take);
            reader.consume(take);
            return Ok(if oversized { max_bytes + 1 } else { total });
        }
        let len = available.len();
        if !oversized && total + len <= max_bytes {
            buf.extend_from_slice(available);
        } else {
            oversized = true;
        }
        total = total.saturating_add(len);
        reader.consume(len);
    }
}

fn supervise_child(
    mut child: Child,
    kill_rx: &std::sync::mpsc::Receiver<()>,
    reaped: &Arc<AtomicBool>,
    shared: &Arc<(Mutex<SharedState>, Condvar)>,
    wake: &Wake,
) {
    let mut kill_requested_at: Option<Instant> = None;
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => break,
            Ok(None) => {}
        }
        if kill_requested_at.is_none() && kill_rx.try_recv().is_ok() {
            kill_requested_at = Some(Instant::now());
        }
        if let Some(since) = kill_requested_at
            && since.elapsed() >= Duration::from_millis(200)
        {
            let _ = child.kill();
            let _ = child.wait();
            break;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    reaped.store(true, Ordering::SeqCst);
    if let Ok(mut state) = shared.0.lock() {
        state.closed = true;
        shared.1.notify_all();
    }
    wake.notify();
}

/// Zeroize every string inside a `serde_json::Value` in place.
pub fn wipe_value(value: &mut Value) {
    match value {
        Value::String(s) => s.zeroize(),
        Value::Array(items) => {
            for item in items {
                wipe_value(item);
            }
        }
        Value::Object(map) => {
            for (_, v) in map {
                wipe_value(v);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

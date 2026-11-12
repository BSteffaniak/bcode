//! Bounded Codex JSONL transport. Process failure is uncertainty, never settlement.
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use zeroize::Zeroize as _;

const MAX_FRAME: usize = 1024 * 1024;
const MAX_PENDING: usize = 32;
const MAX_EVENTS: usize = 1024;
const MAX_EVENT_BYTES: usize = 4 * MAX_FRAME;

/// Sanitized native failure; never contains raw process output or input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CodexError {
    #[error("invalid Codex executable or configuration namespace")]
    Configuration,
    #[error("Codex transport unavailable; reconcile before retrying a mutation")]
    Disconnected,
    #[error("Codex acknowledgment uncertain; mutation must not be replayed")]
    Uncertain,
    #[error("Codex rejected the request")]
    Rejected,
    #[error("Codex protocol bounds exceeded")]
    Bounds,
    #[error("Codex protocol response incompatible")]
    Protocol,
    #[error("stale Codex session generation or native identity")]
    Stale,
    #[error("operation unsupported by this Codex backend")]
    Unsupported,
}

/// Host-admitted native execution selection; paths are never resolved through PATH.
#[derive(Clone)]
pub struct Launch {
    /// Absolute executable.
    pub executable: PathBuf,
    /// Absolute credential/config namespace.
    pub codex_home: PathBuf,
    /// Absolute working directory.
    pub working_directory: PathBuf,
    /// Require verified ephemeral storage for scoped vault credentials.
    pub ephemeral_credentials: bool,
    /// Acknowledgment deadline in 1..=120 seconds.
    pub timeout: Duration,
}
impl Launch {
    fn validate(&self) -> Result<(), CodexError> {
        if !self.executable.is_absolute()
            || !self.codex_home.is_absolute()
            || !self.working_directory.is_absolute()
            || self.timeout.is_zero()
            || self.timeout > Duration::from_secs(120)
        {
            return Err(CodexError::Configuration);
        }
        Ok(())
    }
    fn command(&self) -> Command {
        let mut command = Command::new(&self.executable);
        // Closed allowlist: provider keys, proxy/base-URL routes, preload hooks,
        // remote app-server selectors and future Codex environment switches must
        // not cross this boundary. Native credentials remain under CODEX_HOME.
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LANG", "en_US.UTF-8")
            .env("CODEX_HOME", &self.codex_home)
            .current_dir(&self.working_directory);
        // HOME is needed by native OS credential custody. Derive it from the
        // explicitly admitted default namespace, never the parent environment.
        if self
            .codex_home
            .file_name()
            .is_some_and(|name| name == ".codex")
            && let Some(home) = self.codex_home.parent()
        {
            command.env("HOME", home);
        }
        command
    }
}
/// Change counter signalled whenever native evidence arrives or the transport closes, so a
/// bounded observe wait can return without polling.
#[derive(Default)]
pub(crate) struct WakeInner {
    changes: Mutex<u64>,
    signal: Condvar,
}
/// Shared wake handle for one native transport.
pub(crate) type Wake = Arc<WakeInner>;
impl WakeInner {
    fn notify(&self) {
        if let Ok(mut changes) = self.changes.lock() {
            *changes = changes.wrapping_add(1);
            self.signal.notify_all();
        }
    }
    /// Current change count.
    pub(crate) fn seen(&self) -> u64 {
        self.changes.lock().map_or(0, |changes| *changes)
    }
    /// Wait until the count differs from `seen` or `timeout` elapses; true when changed.
    pub(crate) fn wait_changed(&self, seen: u64, timeout: Duration) -> bool {
        let Ok(changes) = self.changes.lock() else {
            return true;
        };
        self.signal
            .wait_timeout_while(changes, timeout, |changes| *changes == seen)
            .map_or(true, |(changes, _)| *changes != seen)
    }
}

/// Transport readiness for a waiting observer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Readiness {
    /// Nothing queued.
    Idle,
    /// Only streaming deltas are queued; a short coalescing wait is allowed.
    DeltasOnly,
    /// Durable evidence, a callback, a gap or closure is ready.
    Ready,
}

/// Notifications presented only as transient live text; consecutive frames coalesce.
fn is_live_delta(method: &str) -> bool {
    matches!(
        method,
        "item/agentMessage/delta" | "item/reasoning/summaryTextDelta"
    )
}

/// Notifications the adapter interprets. Everything else (output deltas duplicated by item
/// completion, diffs, rate limits, warnings, MCP status, realtime) is dropped at the reader
/// so it cannot exhaust the bounded queue. Server requests are never dropped.
fn interpreted_notification(method: &str) -> bool {
    is_live_delta(method)
        || matches!(
            method,
            "thread/started"
                | "thread/status/changed"
                | "thread/tokenUsage/updated"
                | "turn/started"
                | "turn/completed"
                | "item/started"
                | "item/completed"
                | "serverRequest/resolved"
                | "remoteControl/status/changed"
                | "account/login/completed"
        )
}

struct State {
    pending: BTreeMap<u64, mpsc::SyncSender<Result<Value, CodexError>>>,
    events: VecDeque<Value>,
    event_bytes: usize,
    gap: bool,
    closed: bool,
    write_deadline: Option<Instant>,
    local_only: bool,
    fresh_thread: Option<String>,
    wake: Wake,
}
struct Frame {
    bytes: Vec<u8>,
    deadline: Instant,
}
impl Drop for Frame {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

pub(crate) struct AppServer {
    // Supervisor has sole Child ownership; invocation-thread drop never blocks on wait().
    writer: mpsc::SyncSender<Frame>,
    state: Arc<Mutex<State>>,
    next_id: u64,
    timeout: Duration,
    version: String,
    wake: Wake,
    reaped: Arc<AtomicBool>,
}
impl AppServer {
    pub(crate) fn spawn(launch: &Launch) -> Result<Self, CodexError> {
        launch.validate()?;
        let version = probe_version(launch)?;
        let mut command = launch.command();
        command
            .arg("app-server")
            .arg("--listen")
            .arg("stdio://")
            .arg("-c")
            .arg("features.multi_agent=false")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if launch.ephemeral_credentials {
            command
                .arg("-c")
                .arg("cli_auth_credentials_store=\"ephemeral\"");
        }
        let mut child = command.spawn().map_err(|_| CodexError::Disconnected)?;
        let stdin = child.stdin.take().ok_or(CodexError::Disconnected)?;
        let stdout = child.stdout.take().ok_or(CodexError::Disconnected)?;
        let stderr = child.stderr.take().ok_or(CodexError::Disconnected)?;
        let wake = Wake::default();
        let state = Arc::new(Mutex::new(State {
            pending: BTreeMap::new(),
            events: VecDeque::new(),
            event_bytes: 0,
            gap: false,
            closed: false,
            write_deadline: None,
            local_only: false,
            fresh_thread: None,
            wake: Arc::clone(&wake),
        }));
        // Own/reap the child outside the invocation thread. Drop/cancellation never waits on wait().
        let monitor = Arc::clone(&state);
        let reaped = Arc::new(AtomicBool::new(false));
        supervise_child(child, monitor, Arc::clone(&reaped));
        let (writer, receiver) = mpsc::sync_channel::<Frame>(MAX_PENDING);
        let write_state = Arc::clone(&state);
        thread::spawn(move || {
            let mut stdin = stdin;
            while let Ok(frame) = receiver.recv() {
                // A timed-out call fences the entire transport. Queued frames are withdrawn;
                // a write already in flight remains uncertain and the supervisor kills the process.
                let closed = write_state.lock().map_or(true, |mut state| {
                    if !state.closed {
                        state.write_deadline = Some(frame.deadline);
                    }
                    state.closed
                });
                if closed || Instant::now() >= frame.deadline {
                    disconnect(&write_state);
                    continue;
                }
                let result = stdin.write_all(&frame.bytes).and_then(|()| stdin.flush());
                if let Ok(mut state) = write_state.lock() {
                    state.write_deadline = None;
                }
                if result.is_err() {
                    disconnect(&write_state);
                }
            }
        });
        let read_state = Arc::clone(&state);
        drain_stdout(stdout, read_state);
        thread::spawn(move || {
            let _ = std::io::copy(&mut BufReader::new(stderr), &mut std::io::sink());
        });
        Ok(Self {
            writer,
            state,
            next_id: 1,
            timeout: launch.timeout,
            version,
            wake,
            reaped,
        })
    }

    pub(crate) fn initialize(&mut self, launch: &Launch) -> Result<String, CodexError> {
        let result = self.call(
            "initialize",
            json!({"clientInfo":{"name":"bcode","title":"Bcode",
            "version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}}),
        )?;
        if result["codexHome"].as_str() != launch.codex_home.to_str() {
            return Err(CodexError::Configuration);
        }
        self.send(json!({"method":"initialized"}))?;
        Ok(self.version.clone())
    }

    /// Establish a process-local control policy without changing persisted native preferences.
    /// Arm continuous activation fencing before the final verification RPC.
    pub(crate) fn require_local_only(&mut self) -> Result<(), CodexError> {
        let disabled = self.call("remoteControl/disable", json!({"ephemeral":true}))?;
        if disabled["status"].as_str() != Some("disabled") {
            self.fence();
            return Err(CodexError::Unsupported);
        }
        {
            let mut state = self.state.lock().map_err(|_| CodexError::Disconnected)?;
            state.local_only = true;
            // Catch activation already read between disable acknowledgment and arming.
            if state.events.iter().any(remote_activation) {
                state.gap = true;
                fail_closed(&mut state);
                return Err(CodexError::Uncertain);
            }
            drop(state);
        }
        let status = self.call("remoteControl/status/read", json!({}))?;
        if status["status"].as_str() != Some("disabled") {
            self.fence();
            return Err(CodexError::Unsupported);
        }
        Ok(())
    }

    pub(crate) fn arm_first_input(&self, thread_id: &str) -> Result<(), CodexError> {
        let mut state = self.state.lock().map_err(|_| CodexError::Disconnected)?;
        if state.closed
            || !state.local_only
            || state.events.iter().any(|event| {
                event["method"] == "turn/started"
                    && event["params"]["threadId"].as_str() == Some(thread_id)
            })
        {
            fail_closed(&mut state);
            return Err(CodexError::Uncertain);
        }
        state.fresh_thread = Some(thread_id.to_owned());
        drop(state);
        Ok(())
    }
    pub(crate) fn consume_first_input(&self) -> Result<(), CodexError> {
        let mut state = self.state.lock().map_err(|_| CodexError::Disconnected)?;
        if state.closed {
            return Err(CodexError::Uncertain);
        }
        state.fresh_thread = None;
        drop(state);
        Ok(())
    }

    pub(crate) fn next_request_id(&self) -> String {
        self.next_id.to_string()
    }

    pub(crate) fn call(&mut self, method: &str, params: Value) -> Result<Value, CodexError> {
        let id = self.next_id;
        self.next_id = id.checked_add(1).ok_or(CodexError::Bounds)?;
        let (sender, receiver) = mpsc::sync_channel(1);
        {
            let mut state = self.state.lock().map_err(|_| CodexError::Disconnected)?;
            if state.closed {
                return Err(CodexError::Uncertain);
            }
            if state.pending.len() >= MAX_PENDING {
                return Err(CodexError::Bounds);
            }
            state.pending.insert(id, sender);
        }
        let mut envelope = json!({"id":id,"method":method});
        envelope["params"] = params;
        if let Err(error) = self.send(envelope) {
            disconnect(&self.state);
            return Err(error);
        }
        let result = if let Ok(value) = receiver.recv_timeout(self.timeout) {
            value
        } else {
            disconnect(&self.state);
            Err(CodexError::Uncertain)
        };
        if let Ok(mut state) = self.state.lock() {
            state.pending.remove(&id);
        }
        result
    }

    pub(crate) fn send(&self, mut value: Value) -> Result<(), CodexError> {
        let encoded = serde_json::to_vec(&value);
        wipe_value(&mut value);
        let bytes = encoded.map_err(|_| CodexError::Protocol)?;
        let mut frame = Frame {
            bytes,
            deadline: Instant::now() + self.timeout,
        };
        if frame.bytes.len() > MAX_FRAME {
            return Err(CodexError::Bounds);
        }
        frame.bytes.push(b'\n');
        if self.state.lock().map_or(true, |state| state.closed) {
            return Err(CodexError::Uncertain);
        }
        self.writer.try_send(frame).map_err(|_| {
            disconnect(&self.state);
            CodexError::Uncertain
        })
    }

    pub(crate) fn events(&self, limit: usize) -> Result<(Vec<Value>, bool, bool), CodexError> {
        if limit == 0 || limit > 128 {
            return Err(CodexError::Bounds);
        }
        let mut state = self.state.lock().map_err(|_| CodexError::Disconnected)?;
        let mut events = Vec::new();
        for _ in 0..limit {
            let Some(event) = state.events.pop_front() else {
                break;
            };
            state.event_bytes = state.event_bytes.saturating_sub(event.to_string().len());
            events.push(event);
        }
        Ok((events, state.gap, state.closed))
    }
    /// Wake handle signalled on native evidence or closure.
    pub(crate) fn wake(&self) -> Wake {
        Arc::clone(&self.wake)
    }
    /// Physical reap flag, set only after the supervisor waited on the exited child.
    pub(crate) fn reap_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.reaped)
    }
    /// Whether queued native input is ready for an observer.
    pub(crate) fn readiness(&self) -> Readiness {
        let Ok(state) = self.state.lock() else {
            return Readiness::Ready;
        };
        if state.gap || state.closed {
            return Readiness::Ready;
        }
        if state.events.is_empty() {
            return Readiness::Idle;
        }
        if state.events.iter().all(|event| {
            event.get("id").is_none() && event["method"].as_str().is_some_and(is_live_delta)
        }) {
            Readiness::DeltasOnly
        } else {
            Readiness::Ready
        }
    }
    pub(crate) fn health(&self) -> Result<(bool, bool), CodexError> {
        let state = self.state.lock().map_err(|_| CodexError::Disconnected)?;
        Ok((state.gap, state.closed))
    }
    pub(crate) fn fence(&self) {
        disconnect(&self.state);
    }
}
impl Drop for AppServer {
    fn drop(&mut self) {
        disconnect(&self.state);
    }
}

fn supervise_child(
    mut child: std::process::Child,
    monitor: Arc<Mutex<State>>,
    reaped: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        loop {
            let closed = monitor.lock().map_or(true, |mut state| {
                if state
                    .write_deadline
                    .is_some_and(|deadline| Instant::now() >= deadline)
                {
                    fail_closed(&mut state);
                }
                state.closed
            });
            if closed {
                let _ = child.kill();
                if child.wait().is_ok() {
                    reaped.store(true, Ordering::SeqCst);
                }
                break;
            }
            match child.try_wait() {
                Ok(None) => thread::sleep(Duration::from_millis(5)),
                Ok(Some(_)) => {
                    // try_wait reaped the exited child.
                    reaped.store(true, Ordering::SeqCst);
                    disconnect(&monitor);
                    break;
                }
                Err(_) => {
                    disconnect(&monitor);
                    break;
                }
            }
        }
    });
}

fn drain_stdout(stdout: std::process::ChildStdout, read_state: Arc<Mutex<State>>) {
    thread::spawn(move || {
        let mut stdout = BufReader::new(stdout);
        loop {
            if let Ok(Some(mut frame)) = read_frame(&mut stdout) {
                let decoded = serde_json::from_slice::<Value>(&frame);
                frame.zeroize();
                if let Ok(value) = decoded {
                    receive(&read_state, value);
                } else {
                    disconnect(&read_state);
                    break;
                }
            } else {
                disconnect(&read_state);
                break;
            }
        }
        let _ = std::io::copy(&mut stdout, &mut std::io::sink());
    });
}

// Pinned CLI declares version at the TOP LEVEL; app-server does not propagate it.
// Probe has no provider request, login, or session creation. Output and waiting are bounded.
pub(crate) fn probe_version(launch: &Launch) -> Result<String, CodexError> {
    let mut child = launch
        .command()
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| CodexError::Disconnected)?;
    let stdout = child.stdout.take().ok_or(CodexError::Disconnected)?;
    let stderr = child.stderr.take().ok_or(CodexError::Disconnected)?;
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut bytes = Vec::new();
        let result = (&mut reader).take(513).read_to_end(&mut bytes);
        let _ = sender.send((result.is_ok(), bytes));
        let _ = std::io::copy(&mut reader, &mut std::io::sink());
    });
    thread::spawn(move || {
        let _ = std::io::copy(&mut BufReader::new(stderr), &mut std::io::sink());
    });
    let deadline = Instant::now() + launch.timeout.min(Duration::from_secs(5));
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                thread::spawn(move || {
                    let _ = child.wait();
                });
                return Err(CodexError::Unsupported);
            }
        }
    };
    let (ok, bytes) = receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| CodexError::Unsupported)?;
    if !status.success() || !ok || bytes.len() > 512 {
        return Err(CodexError::Unsupported);
    }
    let output = std::str::from_utf8(&bytes)
        .map_err(|_| CodexError::Unsupported)?
        .trim();
    let version = output
        .strip_prefix("codex-cli ")
        .ok_or(CodexError::Unsupported)?;
    if !well_formed_version(version) {
        return Err(CodexError::Unsupported);
    }
    Ok(version.to_owned())
}

/// Whether `version` has the bounded shape of a probed `codex --version` value.
pub(crate) fn well_formed_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 128
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-+".contains(&b))
}

pub(crate) fn wipe_value(value: &mut Value) {
    match value {
        Value::String(value) => value.zeroize(),
        Value::Array(values) => {
            for value in values {
                wipe_value(value);
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                wipe_value(value);
            }
        }
        _ => {}
    }
}
fn disconnect(state: &Mutex<State>) {
    if let Ok(mut state) = state.lock() {
        fail_closed(&mut state);
    }
}
fn fail_closed(state: &mut State) {
    state.closed = true;
    state.wake.notify();
    for (_, sender) in std::mem::take(&mut state.pending) {
        let _ = sender.try_send(Err(CodexError::Uncertain));
    }
}
fn remote_activation(value: &Value) -> bool {
    value["method"].as_str() == Some("remoteControl/status/changed")
        && value["params"]["status"].as_str() != Some("disabled")
}

fn receive(state: &Mutex<State>, mut value: Value) {
    let Ok(mut state) = state.lock() else {
        return;
    };
    if state.closed {
        wipe_value(&mut value);
        return;
    }
    if (state.local_only && remote_activation(&value))
        || (value["method"] == "turn/started"
            && state
                .fresh_thread
                .as_deref()
                .is_some_and(|id| value["params"]["threadId"].as_str() == Some(id)))
    {
        state.gap = true;
        fail_closed(&mut state);
        wipe_value(&mut value);
        return;
    }
    if value.get("method").is_none() {
        if let Some(id) = value.get("id").and_then(Value::as_u64)
            && let Some(sender) = state.pending.remove(&id)
        {
            let result = if value.get("error").is_some() {
                Err(CodexError::Rejected)
            } else {
                value
                    .get_mut("result")
                    .map(Value::take)
                    .ok_or(CodexError::Protocol)
            };
            let _ = sender.try_send(result);
        }
        wipe_value(&mut value);
        return;
    }
    if value["method"].as_str() == Some("account/login/completed") {
        let sanitized = json!({"method":"account/login/completed","params":{
            "loginId":value["params"]["loginId"],"success":value["params"]["success"]}});
        wipe_value(&mut value);
        value = sanitized;
    } else if value["method"]
        .as_str()
        .is_some_and(|method| method.starts_with("account/"))
    {
        wipe_value(&mut value);
        return;
    }
    let method = value["method"].as_str().unwrap_or_default().to_owned();
    if value.get("id").is_none() && !interpreted_notification(&method) {
        wipe_value(&mut value);
        return;
    }
    if value.get("id").is_none() && is_live_delta(&method) && coalesce_delta(&mut state, &value) {
        state.wake.notify();
        return;
    }
    let bytes = value.to_string().len();
    if state.events.len() >= MAX_EVENTS || state.event_bytes.saturating_add(bytes) > MAX_EVENT_BYTES
    {
        state.gap = true;
        // In particular, never abandon a server request and leave native work waiting:
        // fence pending calls/writes and have the supervisor disconnect the process.
        fail_closed(&mut state);
        wipe_value(&mut value);
        return;
    }
    state.event_bytes += bytes;
    state.events.push_back(value);
    state.wake.notify();
}

/// Append a delta to the immediately preceding queued delta of the same item and part.
/// Ordering is preserved: only adjacent frames merge.
fn coalesce_delta(state: &mut State, value: &Value) -> bool {
    let Some(delta) = value["params"]["delta"].as_str() else {
        return false;
    };
    let event_bytes = state.event_bytes;
    let Some(last) = state.events.back_mut() else {
        return false;
    };
    if last.get("id").is_some()
        || last["method"] != value["method"]
        || last["params"]["threadId"] != value["params"]["threadId"]
        || last["params"]["itemId"] != value["params"]["itemId"]
        || last["params"]["summaryIndex"] != value["params"]["summaryIndex"]
        || event_bytes.saturating_add(delta.len()) > MAX_EVENT_BYTES
    {
        return false;
    }
    let Some(Value::String(text)) = last
        .get_mut("params")
        .and_then(|params| params.get_mut("delta"))
    else {
        return false;
    };
    if text.len().saturating_add(delta.len()) > MAX_FRAME / 2 {
        return false;
    }
    text.push_str(delta);
    state.event_bytes += delta.len();
    true
}
fn read_frame(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>, CodexError> {
    let mut frame = Vec::new();
    loop {
        let bytes = reader.fill_buf().map_err(|_| CodexError::Disconnected)?;
        if bytes.is_empty() {
            return if frame.is_empty() {
                Ok(None)
            } else {
                Err(CodexError::Protocol)
            };
        }
        let count = bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(bytes.len(), |n| n + 1);
        if frame.len().saturating_add(count) > MAX_FRAME {
            frame.zeroize();
            return Err(CodexError::Bounds);
        }
        let done = bytes[count - 1] == b'\n';
        frame.extend_from_slice(&bytes[..count]);
        reader.consume(count);
        if done {
            return Ok(Some(frame));
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn child_environment_is_closed_not_a_provider_denylist() {
        let temp = tempfile::tempdir().unwrap();
        let launch = Launch {
            executable: PathBuf::from("/usr/bin/env"),
            codex_home: temp.path().join(".codex"),
            working_directory: temp.path().to_owned(),
            ephemeral_credentials: false,
            timeout: Duration::from_secs(1),
        };
        let output = launch.command().output().unwrap();
        assert!(output.status.success());
        let actual: BTreeMap<_, _> = std::str::from_utf8(&output.stdout)
            .unwrap()
            .lines()
            .map(|line| line.split_once('=').unwrap())
            .collect();
        assert_eq!(
            actual,
            BTreeMap::from([
                ("PATH", "/usr/bin:/bin"),
                ("LANG", "en_US.UTF-8"),
                ("HOME", temp.path().to_str().unwrap()),
                ("CODEX_HOME", launch.codex_home.to_str().unwrap()),
            ])
        );
    }
    #[test]
    fn frame_bounds_and_truncated_eof_are_not_success() {
        assert!(read_frame(&mut &b"{}\n"[..]).unwrap().is_some());
        assert_eq!(read_frame(&mut &b"{"[..]), Err(CodexError::Protocol));
        assert_eq!(
            read_frame(&mut vec![b'x'; MAX_FRAME + 1].as_slice()),
            Err(CodexError::Bounds)
        );
    }
    #[test]
    fn unexpected_remote_activation_fences_without_observe_polling() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let state = Mutex::new(State {
            pending: BTreeMap::from([(1, sender)]),
            events: VecDeque::new(),
            event_bytes: 0,
            gap: false,
            closed: false,
            write_deadline: None,
            local_only: true,
            fresh_thread: Some("fresh".to_owned()),
            wake: Wake::default(),
        });
        receive(
            &state,
            json!({"method":"remoteControl/status/changed","params":{"status":"connected"}}),
        );
        assert_eq!(receiver.recv().unwrap(), Err(CodexError::Uncertain));
        let state = state.lock().unwrap();
        assert!(state.closed && state.gap);
        assert!(state.events.is_empty());
        drop(state);
    }
    #[test]
    fn unexpected_start_before_consumption_revokes_fresh_construction() {
        let state = Mutex::new(State {
            pending: BTreeMap::new(),
            events: VecDeque::new(),
            event_bytes: 0,
            gap: false,
            closed: false,
            write_deadline: None,
            local_only: true,
            fresh_thread: Some("fresh".to_owned()),
            wake: Wake::default(),
        });
        receive(
            &state,
            json!({"method":"turn/started","params":{"threadId":"fresh","turn":{"id":"rogue"}}}),
        );
        let state = state.lock().unwrap();
        assert!(state.closed && state.gap);
        assert!(state.events.is_empty());
        drop(state);
    }
    #[test]
    fn callback_overflow_fences_pending_and_closes_transport() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let state = Mutex::new(State {
            pending: BTreeMap::from([(1, sender)]),
            events: VecDeque::new(),
            event_bytes: 0,
            gap: false,
            closed: false,
            write_deadline: None,
            local_only: false,
            fresh_thread: None,
            wake: Wake::default(),
        });
        for _ in 0..=MAX_EVENTS {
            receive(
                &state,
                json!({"id":91,"method":"item/tool/requestUserInput","params":{}}),
            );
        }
        let state = state.lock().unwrap();
        assert!(state.gap && state.closed);
        assert_eq!(receiver.recv().unwrap(), Err(CodexError::Uncertain));
        assert_eq!(state.events.len(), MAX_EVENTS);
    }
}

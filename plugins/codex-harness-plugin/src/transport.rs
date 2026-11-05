//! Bounded Codex JSONL transport. Process failure is uncertainty, never settlement.
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use zeroize::Zeroize as _;

const MAX_FRAME: usize = 1024 * 1024;
const MAX_PENDING: usize = 32;
const MAX_EVENTS: usize = 256;
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
        command
            .env("CODEX_HOME", &self.codex_home)
            .env_remove("OPENAI_API_KEY")
            .env_remove("CODEX_API_KEY")
            .env_remove("OPENAI_BASE_URL")
            .current_dir(&self.working_directory);
        command
    }
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
        let state = Arc::new(Mutex::new(State {
            pending: BTreeMap::new(),
            events: VecDeque::new(),
            event_bytes: 0,
            gap: false,
            closed: false,
            write_deadline: None,
            local_only: false,
            fresh_thread: None,
        }));
        // Own/reap the child outside the invocation thread. Drop/cancellation never waits on wait().
        let monitor = Arc::clone(&state);
        supervise_child(child, monitor);
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

fn supervise_child(mut child: std::process::Child, monitor: Arc<Mutex<State>>) {
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
                let _ = child.wait();
                break;
            }
            if matches!(child.try_wait(), Ok(None)) {
                thread::sleep(Duration::from_millis(5));
            } else {
                disconnect(&monitor);
                break;
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
fn probe_version(launch: &Launch) -> Result<String, CodexError> {
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
    if version.is_empty()
        || version.len() > 128
        || !version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-+".contains(&b))
    {
        return Err(CodexError::Unsupported);
    }
    Ok(version.to_owned())
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
